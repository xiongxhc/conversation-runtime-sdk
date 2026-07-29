#[allow(
    dead_code,
    reason = "the private codec is consumed by the next sidecar process module"
)]
pub(crate) mod codec;

#[cfg(test)]
mod codec_tests;
