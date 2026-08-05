export class AsyncQueue<T> implements AsyncIterable<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<{
    resolve: (result: IteratorResult<T>) => void;
    reject: (error: Error) => void;
  }> = [];
  private error: Error | undefined;
  private finished = false;

  push(value: T): void {
    if (this.finished) {
      return;
    }
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve({ value, done: false });
      return;
    }
    this.values.push(value);
  }

  finish(): void {
    this.end();
  }

  fail(error: Error): void {
    if (this.finished) {
      return;
    }
    this.values.length = 0;
    this.error = error;
    this.end();
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: () => {
        if (this.error) {
          return Promise.reject(this.error);
        }
        const value = this.values.shift();
        if (value !== undefined) {
          return Promise.resolve({ value, done: false });
        }
        if (this.finished) {
          return Promise.resolve({ value: undefined, done: true });
        }
        return new Promise<IteratorResult<T>>((resolve, reject) => this.waiters.push({ resolve, reject }));
      },
    };
  }

  private end(): void {
    if (this.finished) {
      return;
    }
    this.finished = true;
    for (const waiter of this.waiters.splice(0)) {
      if (this.error) {
        waiter.reject(this.error);
      } else {
        waiter.resolve({ value: undefined, done: true });
      }
    }
  }
}
