#define _DARWIN_C_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/param.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t stop_requested;

static void request_stop(int signal_number) {
    (void)signal_number;
    stop_requested = 1;
}

static int sleep_milliseconds(unsigned int milliseconds) {
    struct timespec requested = {
        .tv_sec = milliseconds / 1000,
        .tv_nsec = (long)(milliseconds % 1000) * 1000000L,
    };

    while (nanosleep(&requested, &requested) == -1) {
        if (errno != EINTR) {
            return -1;
        }
        if (stop_requested) {
            errno = EINTR;
            return -1;
        }
    }
    return 0;
}

static int parse_positive_pid(const char *text, pid_t *result) {
    char *end = NULL;
    errno = 0;
    long parsed = strtol(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || parsed <= 0 ||
        parsed > INT32_MAX) {
        return -1;
    }
    *result = (pid_t)parsed;
    return 0;
}

static int write_all(int descriptor, const void *buffer, size_t length) {
    const unsigned char *cursor = buffer;
    size_t remaining = length;
    while (remaining > 0) {
        ssize_t written = write(descriptor, cursor, remaining);
        if (written == -1) {
            if (errno == EINTR && !stop_requested) {
                continue;
            }
            return -1;
        }
        if (written == 0) {
            errno = EIO;
            return -1;
        }
        cursor += (size_t)written;
        remaining -= (size_t)written;
    }
    return 0;
}

static int write_exclusive_file(const char *path, const char *content) {
    int descriptor =
        open(path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600);
    if (descriptor == -1) {
        return -1;
    }
    size_t length = strlen(content);
    int result = write_all(descriptor, content, length);
    if (result == 0 && fsync(descriptor) == -1) {
        result = -1;
    }
    int saved_errno = errno;
    if (close(descriptor) == -1 && result == 0) {
        result = -1;
        saved_errno = errno;
    }
    errno = saved_errno;
    return result;
}

static int write_atomic_file(const char *path, const char *content) {
    char temporary[MAXPATHLEN];
    int length = snprintf(
        temporary, sizeof(temporary), "%s.tmp.%ld", path, (long)getpid());
    if (length < 0 || (size_t)length >= sizeof(temporary)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    if (write_exclusive_file(temporary, content) == -1) {
        return -1;
    }
    if (rename(temporary, path) == -1) {
        int saved_errno = errno;
        unlink(temporary);
        errno = saved_errno;
        return -1;
    }
    return 0;
}

static bool same_identity(const struct stat *left, const struct stat *right) {
    return left->st_dev == right->st_dev && left->st_ino == right->st_ino;
}

static int open_directory_securely(const char *path, struct stat *identity) {
    if (path[0] != '/') {
        errno = EINVAL;
        return -1;
    }
    size_t path_length = strlen(path);
    if (path_length >= MAXPATHLEN) {
        errno = ENAMETOOLONG;
        return -1;
    }

    char copy[MAXPATHLEN];
    memcpy(copy, path, path_length + 1);
    int directory =
        open("/", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (directory == -1) {
        return -1;
    }

    char *cursor = copy;
    while (*cursor == '/') {
        cursor++;
    }
    while (*cursor != '\0') {
        char *separator = strchr(cursor, '/');
        if (separator != NULL) {
            *separator = '\0';
        }
        if (strcmp(cursor, ".") == 0 || strcmp(cursor, "..") == 0) {
            close(directory);
            errno = EINVAL;
            return -1;
        }
        int next = openat(
            directory,
            cursor,
            O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
        if (next == -1) {
            int saved_errno = errno;
            close(directory);
            errno = saved_errno;
            return -1;
        }
        close(directory);
        directory = next;

        if (separator == NULL) {
            break;
        }
        cursor = separator + 1;
        while (*cursor == '/') {
            cursor++;
        }
    }

    if (fstat(directory, identity) == -1) {
        int saved_errno = errno;
        close(directory);
        errno = saved_errno;
        return -1;
    }
    if (!S_ISDIR(identity->st_mode)) {
        close(directory);
        errno = ENOTDIR;
        return -1;
    }
    return directory;
}

static int directory_contains_identity(
    int directory,
    const struct stat *candidate) {
    int current = dup(directory);
    if (current == -1) {
        return -1;
    }

    for (unsigned int depth = 0; depth < 1024; depth++) {
        struct stat current_identity;
        if (fstat(current, &current_identity) == -1) {
            close(current);
            return -1;
        }
        if (same_identity(&current_identity, candidate)) {
            close(current);
            return 1;
        }

        int parent = openat(
            current, "..", O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
        if (parent == -1) {
            close(current);
            return -1;
        }
        struct stat parent_identity;
        if (fstat(parent, &parent_identity) == -1) {
            close(parent);
            close(current);
            return -1;
        }
        if (same_identity(&current_identity, &parent_identity)) {
            close(parent);
            close(current);
            return 0;
        }
        close(current);
        current = parent;
    }

    close(current);
    errno = ELOOP;
    return -1;
}

static int watch_descriptor(
    int queue,
    int descriptor,
    uint32_t notifications) {
    struct kevent change;
    EV_SET(
        &change,
        (uintptr_t)descriptor,
        EVFILT_VNODE,
        EV_ADD | EV_CLEAR,
        notifications,
        0,
        NULL);
    return kevent(queue, &change, 1, NULL, 0, NULL);
}

static int poll_security_events(
    int queue,
    int parent_descriptor,
    int output_descriptor,
    unsigned int timeout_ms) {
    struct kevent events[4];
    struct timespec timeout = {
        .tv_sec = timeout_ms / 1000,
        .tv_nsec = (long)(timeout_ms % 1000) * 1000000L,
    };
    int event_count;
    do {
        event_count = kevent(queue, NULL, 0, events, 4, &timeout);
    } while (event_count == -1 && errno == EINTR && !stop_requested);
    if (event_count == -1) {
        return -1;
    }
    for (int index = 0; index < event_count; index++) {
        if ((int)events[index].ident == parent_descriptor &&
            (events[index].fflags &
             (NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE)) != 0) {
            errno = ESTALE;
            return -1;
        }
        if ((int)events[index].ident == output_descriptor &&
            (events[index].fflags & NOTE_LINK) != 0) {
            errno = EMLINK;
            return -1;
        }
    }
    return stop_requested ? -1 : 0;
}

static int verify_private_regular_file(
    int descriptor,
    const struct stat *expected) {
    struct stat actual;
    if (fstat(descriptor, &actual) == -1 || !S_ISREG(actual.st_mode) ||
        actual.st_nlink != 1 || (actual.st_mode & 0777) != 0600) {
        errno = EPERM;
        return -1;
    }
    if (expected != NULL && !same_identity(&actual, expected)) {
        errno = ESTALE;
        return -1;
    }
    return 0;
}

#ifdef ACCEPTANCE_HELPER_TESTING
static unsigned int testing_milliseconds(
    const char *name,
    unsigned int fallback,
    unsigned int maximum) {
    const char *value = getenv(name);
    if (value == NULL || *value == '\0') {
        return fallback;
    }
    char *end = NULL;
    errno = 0;
    unsigned long parsed = strtoul(value, &end, 10);
    if (errno != 0 || end == value || *end != '\0' || parsed > maximum) {
        return fallback;
    }
    return (unsigned int)parsed;
}

static int testing_marker(const char *name) {
    const char *path = getenv(name);
    if (path == NULL || *path == '\0') {
        return 0;
    }
    return write_exclusive_file(path, "");
}
#endif

static int metrics_command(int argument_count, char **arguments) {
    if (argument_count != 8) {
        return 64;
    }
    const char *parent_path = arguments[2];
    const char *leaf = arguments[3];
    const char *fifo_path = arguments[4];
    const char *ready_path = arguments[5];
    const char *failed_path = arguments[6];
    const char *repository_path = arguments[7];
    if (*leaf == '\0' || strcmp(leaf, ".") == 0 || strcmp(leaf, "..") == 0 ||
        strchr(leaf, '/') != NULL) {
        return 64;
    }

    struct sigaction stop_action = {
        .sa_handler = request_stop,
    };
    sigemptyset(&stop_action.sa_mask);
    sigaction(SIGHUP, &stop_action, NULL);
    sigaction(SIGINT, &stop_action, NULL);
    sigaction(SIGTERM, &stop_action, NULL);

    int result = 1;
    int parent = -1;
    int repository = -1;
    int stage = -1;
    int output = -1;
    int input = -1;
    int queue = -1;
    bool published = false;
    bool stage_created = false;
    char stage_name[64] = "";
    struct stat parent_identity;
    struct stat repository_identity;
    struct stat output_identity;

    parent = open_directory_securely(parent_path, &parent_identity);
    if (parent == -1 || parent_identity.st_uid != geteuid() ||
        (parent_identity.st_mode & (S_IWGRP | S_IWOTH)) != 0) {
        goto cleanup;
    }
    repository =
        open_directory_securely(repository_path, &repository_identity);
    if (repository == -1) {
        goto cleanup;
    }
    int inside_repository =
        directory_contains_identity(parent, &repository_identity);
    if (inside_repository != 0) {
        goto cleanup;
    }

    struct stat existing;
    if (fstatat(parent, leaf, &existing, AT_SYMLINK_NOFOLLOW) == 0 ||
        errno != ENOENT) {
        goto cleanup;
    }

    queue = kqueue();
    if (queue == -1 ||
        watch_descriptor(
            queue,
            parent,
            NOTE_DELETE | NOTE_RENAME | NOTE_REVOKE) == -1) {
        goto cleanup;
    }

    for (unsigned int attempt = 0; attempt < 128; attempt++) {
        int length = snprintf(
            stage_name,
            sizeof(stage_name),
            ".conversation-runtime-metrics-%08x-%02x",
            arc4random(),
            attempt);
        if (length < 0 || (size_t)length >= sizeof(stage_name)) {
            errno = ENAMETOOLONG;
            goto cleanup;
        }
        if (mkdirat(parent, stage_name, 0700) == 0) {
            stage_created = true;
            break;
        }
        if (errno != EEXIST) {
            goto cleanup;
        }
    }
    if (!stage_created) {
        errno = EEXIST;
        goto cleanup;
    }
    stage = openat(
        parent,
        stage_name,
        O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (stage == -1) {
        goto cleanup;
    }
    struct stat stage_identity;
    if (fstat(stage, &stage_identity) == -1 ||
        stage_identity.st_uid != geteuid() ||
        (stage_identity.st_mode & 0777) != 0700) {
        errno = EPERM;
        goto cleanup;
    }

    output = openat(
        stage,
        "metrics",
        O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,
        0600);
    if (output == -1 || fchmod(output, 0600) == -1 ||
        fstat(output, &output_identity) == -1 ||
        verify_private_regular_file(output, &output_identity) == -1 ||
        watch_descriptor(queue, output, NOTE_LINK) == -1) {
        goto cleanup;
    }

    if (write_exclusive_file(ready_path, "") == -1) {
        goto cleanup;
    }
#ifdef ACCEPTANCE_HELPER_TESTING
    if (testing_marker(
            "CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_MARKER") == -1 ||
        sleep_milliseconds(testing_milliseconds(
            "CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_DELAY_MS",
            0,
            5000)) == -1) {
        goto cleanup;
    }
#endif

    input = open(fifo_path, O_RDONLY | O_CLOEXEC);
    if (input == -1) {
        goto cleanup;
    }
    unsigned char buffer[8192];
    while (!stop_requested) {
        if (poll_security_events(queue, parent, output, 0) == -1 ||
            verify_private_regular_file(output, &output_identity) == -1) {
            goto cleanup;
        }
        ssize_t read_count = read(input, buffer, sizeof(buffer));
        if (read_count == -1) {
            if (errno == EINTR && !stop_requested) {
                continue;
            }
            goto cleanup;
        }
        if (read_count == 0) {
            break;
        }
        if (write_all(output, buffer, (size_t)read_count) == -1 ||
            poll_security_events(queue, parent, output, 0) == -1 ||
            verify_private_regular_file(output, &output_identity) == -1) {
            goto cleanup;
        }
    }
    if (stop_requested || fsync(output) == -1 ||
        poll_security_events(queue, parent, output, 0) == -1 ||
        verify_private_regular_file(output, &output_identity) == -1) {
        goto cleanup;
    }

    struct stat current_parent_identity;
    int current_parent =
        open_directory_securely(parent_path, &current_parent_identity);
    if (current_parent == -1 ||
        !same_identity(&parent_identity, &current_parent_identity)) {
        if (current_parent != -1) {
            close(current_parent);
        }
        errno = ESTALE;
        goto cleanup;
    }
    close(current_parent);
    if (fstatat(parent, leaf, &existing, AT_SYMLINK_NOFOLLOW) == 0 ||
        errno != ENOENT) {
        goto cleanup;
    }
    if (renameatx_np(stage, "metrics", parent, leaf, RENAME_EXCL) == -1) {
        goto cleanup;
    }
    published = true;
    if (fsync(parent) == -1) {
        goto cleanup;
    }

#ifdef ACCEPTANCE_HELPER_TESTING
    if (testing_marker(
            "CONVERSATION_ACCEPTANCE_TEST_METRICS_PUBLISHED_MARKER") == -1) {
        goto cleanup;
    }
    unsigned int monitor_ms = testing_milliseconds(
        "CONVERSATION_ACCEPTANCE_TEST_METRICS_MONITOR_MS", 100, 5000);
#else
    unsigned int monitor_ms = 100;
#endif
    if (poll_security_events(queue, parent, output, monitor_ms) == -1 ||
        verify_private_regular_file(output, &output_identity) == -1) {
        goto cleanup;
    }

    current_parent =
        open_directory_securely(parent_path, &current_parent_identity);
    if (current_parent == -1 ||
        !same_identity(&parent_identity, &current_parent_identity)) {
        if (current_parent != -1) {
            close(current_parent);
        }
        errno = ESTALE;
        goto cleanup;
    }
    close(current_parent);

    struct stat published_identity;
    if (fstatat(parent, leaf, &published_identity, AT_SYMLINK_NOFOLLOW) == -1 ||
        !same_identity(&published_identity, &output_identity) ||
        !S_ISREG(published_identity.st_mode) ||
        published_identity.st_nlink != 1 ||
        (published_identity.st_mode & 0777) != 0600) {
        errno = ESTALE;
        goto cleanup;
    }
    result = 0;

cleanup:
    if (result != 0) {
        int ignored = write_exclusive_file(failed_path, "");
        (void)ignored;
    }
    if (published) {
        if (result != 0) {
            unlinkat(parent, leaf, 0);
        }
    } else if (stage != -1) {
        unlinkat(stage, "metrics", 0);
    }
    if (input != -1) {
        close(input);
    }
    if (output != -1) {
        close(output);
    }
    if (stage != -1) {
        close(stage);
    }
    if (stage_created && parent != -1) {
        unlinkat(parent, stage_name, AT_REMOVEDIR);
    }
    if (queue != -1) {
        close(queue);
    }
    if (repository != -1) {
        close(repository);
    }
    if (parent != -1) {
        close(parent);
    }
    return result;
}

static int verify_session_identity(pid_t process, pid_t group, pid_t session) {
    if (process <= 0 || process != group || process != session) {
        errno = EINVAL;
        return -1;
    }
    pid_t actual_group = getpgid(process);
    if (actual_group == -1 || actual_group != group) {
        errno = ESRCH;
        return -1;
    }
    pid_t actual_session = getsid(process);
    if (actual_session == -1 || actual_session != session) {
        errno = ESRCH;
        return -1;
    }
    return 0;
}

static int wait_for_path(const char *path) {
    while (true) {
        struct stat state;
        if (lstat(path, &state) == 0) {
            return S_ISREG(state.st_mode) ? 0 : -1;
        }
        if (errno != ENOENT) {
            return -1;
        }
        if (sleep_milliseconds(10) == -1) {
            return -1;
        }
    }
}

static void reset_child_signals(void) {
    struct sigaction default_action = {
        .sa_handler = SIG_DFL,
    };
    sigemptyset(&default_action.sa_mask);
    sigaction(SIGHUP, &default_action, NULL);
    sigaction(SIGINT, &default_action, NULL);
    sigaction(SIGTERM, &default_action, NULL);
}

static int launch_command(int argument_count, char **arguments) {
    if (argument_count < 7) {
        return 64;
    }
    const char *handshake_path = arguments[2];
    const char *release_path = arguments[3];
    const char *status_path = arguments[4];
    const char *finish_path = arguments[5];

#ifdef ACCEPTANCE_HELPER_TESTING
    const char *launch_mode =
        getenv("CONVERSATION_ACCEPTANCE_TEST_LAUNCH_MODE");
    if (launch_mode != NULL && strcmp(launch_mode, "setsid_failure") == 0) {
        if (setpgid(0, 0) == -1 || setsid() != -1) {
            return 1;
        }
        return 1;
    }
#endif

    if (setsid() == -1) {
        return 1;
    }

#ifdef ACCEPTANCE_HELPER_TESTING
    if (launch_mode != NULL && strcmp(launch_mode, "delay") == 0 &&
        sleep_milliseconds(1500) == -1) {
        return 1;
    }
#endif

    pid_t process = getpid();
    pid_t group = getpgrp();
    pid_t session = getsid(0);
    pid_t reported_group = group;
    pid_t reported_session = session;
#ifdef ACCEPTANCE_HELPER_TESTING
    if (launch_mode != NULL && strcmp(launch_mode, "mismatch") == 0) {
        reported_group = group + 1;
    } else if (
        launch_mode != NULL && strcmp(launch_mode, "collision") == 0) {
        const char *collision =
            getenv("CONVERSATION_ACCEPTANCE_TEST_COLLISION_ID");
        if (collision == NULL ||
            parse_positive_pid(collision, &reported_group) == -1) {
            return 1;
        }
        reported_session = reported_group;
    }
#endif

    char handshake[128];
    int handshake_length = snprintf(
        handshake,
        sizeof(handshake),
        "%ld %ld %ld\n",
        (long)process,
        (long)reported_group,
        (long)reported_session);
    if (handshake_length < 0 ||
        (size_t)handshake_length >= sizeof(handshake) ||
        write_atomic_file(handshake_path, handshake) == -1 ||
        wait_for_path(release_path) == -1) {
        return 1;
    }

    struct sigaction ignored_action = {
        .sa_handler = SIG_IGN,
    };
    sigemptyset(&ignored_action.sa_mask);
    sigaction(SIGHUP, &ignored_action, NULL);
    sigaction(SIGINT, &ignored_action, NULL);
    sigaction(SIGTERM, &ignored_action, NULL);

    pid_t measured = fork();
    if (measured == -1) {
        return 1;
    }
    if (measured == 0) {
        reset_child_signals();
        execv(arguments[6], &arguments[6]);
        _exit(127);
    }

    int wait_status;
    while (waitpid(measured, &wait_status, 0) == -1) {
        if (errno != EINTR) {
            return 1;
        }
    }
    int command_status;
    if (WIFEXITED(wait_status)) {
        command_status = WEXITSTATUS(wait_status);
    } else if (WIFSIGNALED(wait_status)) {
        command_status = 128 + WTERMSIG(wait_status);
    } else {
        command_status = 125;
    }

    char status[64];
    int status_length =
        snprintf(status, sizeof(status), "%d\n", command_status);
    if (status_length < 0 || (size_t)status_length >= sizeof(status) ||
        write_atomic_file(status_path, status) == -1 ||
        wait_for_path(finish_path) == -1) {
        return 1;
    }
    return command_status;
}

static int verify_session_command(int argument_count, char **arguments) {
    if (argument_count != 5) {
        return 64;
    }
    pid_t process;
    pid_t group;
    pid_t session;
    if (parse_positive_pid(arguments[2], &process) == -1 ||
        parse_positive_pid(arguments[3], &group) == -1 ||
        parse_positive_pid(arguments[4], &session) == -1) {
        return 64;
    }
    return verify_session_identity(process, group, session) == 0 ? 0 : 1;
}

static int wait_handshake_command(int argument_count, char **arguments) {
    if (argument_count != 5) {
        return 64;
    }
    pid_t process;
    if (parse_positive_pid(arguments[2], &process) == -1) {
        return 64;
    }
    char *end = NULL;
    errno = 0;
    unsigned long timeout_ms = strtoul(arguments[4], &end, 10);
    if (errno != 0 || end == arguments[4] || *end != '\0' ||
        timeout_ms == 0 || timeout_ms > 60000) {
        return 64;
    }

    struct timespec start;
    if (clock_gettime(CLOCK_MONOTONIC, &start) == -1) {
        return 1;
    }
    uint64_t deadline_ns =
        (uint64_t)start.tv_sec * 1000000000ULL +
        (uint64_t)start.tv_nsec + timeout_ms * 1000000ULL;

    while (true) {
        struct stat state;
        if (lstat(arguments[3], &state) == 0) {
            return S_ISREG(state.st_mode) ? 0 : 1;
        }
        if (errno != ENOENT) {
            return 1;
        }
        if (kill(process, 0) == -1 && errno == ESRCH) {
            return 1;
        }

        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now) == -1) {
            return 1;
        }
        uint64_t now_ns =
            (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec;
        if (now_ns >= deadline_ns) {
            return 1;
        }
        if (sleep_milliseconds(10) == -1) {
            return 1;
        }
    }
}

static int signal_group_command(int argument_count, char **arguments) {
    if (argument_count != 6) {
        return 64;
    }
    pid_t process;
    pid_t group;
    pid_t session;
    if (parse_positive_pid(arguments[2], &process) == -1 ||
        parse_positive_pid(arguments[3], &group) == -1 ||
        parse_positive_pid(arguments[4], &session) == -1) {
        return 64;
    }
    int signal_number;
    if (strcmp(arguments[5], "INT") == 0) {
        signal_number = SIGINT;
    } else if (strcmp(arguments[5], "TERM") == 0) {
        signal_number = SIGTERM;
    } else if (strcmp(arguments[5], "KILL") == 0) {
        signal_number = SIGKILL;
    } else {
        return 64;
    }
    if (verify_session_identity(process, group, session) == -1) {
        return 1;
    }
    return kill(-group, signal_number) == 0 ? 0 : 1;
}

static int mark_watchdog_failure(const char *path) {
    if (write_exclusive_file(path, "") == 0 || errno == EEXIST) {
        return 1;
    }
    return 1;
}

static int signal_verified_group(
    pid_t process,
    pid_t group,
    pid_t session,
    int signal_number,
    const char *failure_path) {
    if (verify_session_identity(process, group, session) == -1) {
        return 0;
    }
    if (kill(-group, signal_number) == -1) {
        return mark_watchdog_failure(failure_path);
    }
    return 0;
}

static int timeout_watchdog_command(int argument_count, char **arguments) {
    if (argument_count != 8) {
        return 64;
    }
    pid_t process;
    pid_t group;
    pid_t session;
    if (parse_positive_pid(arguments[2], &process) == -1 ||
        parse_positive_pid(arguments[3], &group) == -1 ||
        parse_positive_pid(arguments[4], &session) == -1) {
        return 64;
    }
    char *end = NULL;
    errno = 0;
    unsigned long duration = strtoul(arguments[5], &end, 10);
    if (errno != 0 || end == arguments[5] || *end != '\0' ||
        duration == 0 || duration > 86400) {
        return 64;
    }

    struct sigaction stop_action = {
        .sa_handler = request_stop,
    };
    sigemptyset(&stop_action.sa_mask);
    sigaction(SIGHUP, &stop_action, NULL);
    sigaction(SIGINT, &stop_action, NULL);
    sigaction(SIGTERM, &stop_action, NULL);

    if (sleep_milliseconds((unsigned int)duration * 1000) == -1 ||
        verify_session_identity(process, group, session) == -1) {
        return 0;
    }
    if (write_exclusive_file(arguments[6], "") == -1 ||
        signal_verified_group(
            process, group, session, SIGINT, arguments[7]) != 0) {
        return mark_watchdog_failure(arguments[7]);
    }
    if (sleep_milliseconds(10000) == -1) {
        return 0;
    }
    if (signal_verified_group(
            process, group, session, SIGTERM, arguments[7]) != 0) {
        return 1;
    }
    if (sleep_milliseconds(2000) == -1) {
        return 0;
    }
    return signal_verified_group(
        process, group, session, SIGKILL, arguments[7]);
}

static int shutdown_watchdog_command(int argument_count, char **arguments) {
    if (argument_count != 6) {
        return 64;
    }
    pid_t process;
    pid_t group;
    pid_t session;
    if (parse_positive_pid(arguments[2], &process) == -1 ||
        parse_positive_pid(arguments[3], &group) == -1 ||
        parse_positive_pid(arguments[4], &session) == -1) {
        return 64;
    }

    struct sigaction stop_action = {
        .sa_handler = request_stop,
    };
    sigemptyset(&stop_action.sa_mask);
    sigaction(SIGHUP, &stop_action, NULL);
    sigaction(SIGINT, &stop_action, NULL);
    sigaction(SIGTERM, &stop_action, NULL);

    if (sleep_milliseconds(10000) == -1) {
        return 0;
    }
    if (signal_verified_group(
            process, group, session, SIGTERM, arguments[5]) != 0) {
        return 1;
    }
    if (sleep_milliseconds(2000) == -1) {
        return 0;
    }
    return signal_verified_group(
        process, group, session, SIGKILL, arguments[5]);
}

static int group_count_command(int argument_count, char **arguments) {
    if (argument_count != 3) {
        return 64;
    }
    pid_t group;
    if (parse_positive_pid(arguments[2], &group) == -1) {
        return 64;
    }

    int query[] = {CTL_KERN, KERN_PROC, KERN_PROC_PGRP, group};
    size_t length = 0;
    if (sysctl(query, 4, NULL, &length, NULL, 0) == -1) {
        return 1;
    }
    struct kinfo_proc *processes = NULL;
    while (true) {
        if (length == 0) {
            printf("0\n");
            return 0;
        }
        processes = malloc(length);
        if (processes == NULL) {
            return 1;
        }
        size_t available = length;
        if (sysctl(query, 4, processes, &available, NULL, 0) == 0) {
            printf("%zu\n", available / sizeof(*processes));
            free(processes);
            return 0;
        }
        int saved_errno = errno;
        free(processes);
        processes = NULL;
        if (saved_errno != ENOMEM) {
            return 1;
        }
        if (sysctl(query, 4, NULL, &length, NULL, 0) == -1) {
            return 1;
        }
    }
}

#ifdef ACCEPTANCE_HELPER_TESTING
static int test_session_command(int argument_count, char **arguments) {
    if (argument_count != 3 || setsid() == -1) {
        return 64;
    }
    pid_t identity = getpid();
    if (identity != getpgrp() || identity != getsid(0)) {
        return 1;
    }
    char content[64];
    int length = snprintf(content, sizeof(content), "%ld\n", (long)identity);
    if (length < 0 || (size_t)length >= sizeof(content) ||
        write_atomic_file(arguments[2], content) == -1) {
        return 1;
    }
    while (true) {
        pause();
    }
}
#endif

int main(int argument_count, char **arguments) {
    if (argument_count < 2) {
        return 64;
    }
    if (strcmp(arguments[1], "metrics") == 0) {
        return metrics_command(argument_count, arguments);
    }
    if (strcmp(arguments[1], "launch") == 0) {
        return launch_command(argument_count, arguments);
    }
    if (strcmp(arguments[1], "verify-session") == 0) {
        return verify_session_command(argument_count, arguments);
    }
    if (strcmp(arguments[1], "wait-handshake") == 0) {
        return wait_handshake_command(argument_count, arguments);
    }
    if (strcmp(arguments[1], "signal-group") == 0) {
        return signal_group_command(argument_count, arguments);
    }
    if (strcmp(arguments[1], "timeout-watchdog") == 0) {
        return timeout_watchdog_command(argument_count, arguments);
    }
    if (strcmp(arguments[1], "shutdown-watchdog") == 0) {
        return shutdown_watchdog_command(argument_count, arguments);
    }
    if (strcmp(arguments[1], "group-count") == 0) {
        return group_count_command(argument_count, arguments);
    }
#ifdef ACCEPTANCE_HELPER_TESTING
    if (strcmp(arguments[1], "test-session") == 0) {
        return test_session_command(argument_count, arguments);
    }
#endif
    return 64;
}
