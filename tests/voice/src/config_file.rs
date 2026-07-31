use std::io::Read;
use std::path::Path;

use serde::de::DeserializeOwned;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

pub fn load_toml<T>(path: &Path) -> Result<T, String>
where
    T: DeserializeOwned,
{
    if !path.is_absolute() {
        return Err("voice configuration path must be absolute".to_owned());
    }

    let file = std::fs::File::open(path)
        .map_err(|_| "voice configuration file could not be opened".to_owned())?;
    let mut contents = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|_| "voice configuration file could not be read".to_owned())?;
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return Err("voice configuration file exceeded 64 KiB".to_owned());
    }
    let contents = std::str::from_utf8(&contents)
        .map_err(|_| "voice configuration file was not valid UTF-8".to_owned())?;
    toml::from_str(contents).map_err(|_| "voice configuration file was not valid TOML".to_owned())
}
