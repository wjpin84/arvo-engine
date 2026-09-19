//! How a front end finds the engine. The file itself is defined in
//! [`arvo_client::discovery`], which the window reads too; the token is made here,
//! because only the engine writes one.

pub use arvo_client::discovery::{
    read, read_control, remove_if_ours, running, write, write_control, Discovery, CONTROL_FILE, FILE,
};

/// A fresh token: 32 random bytes, as hex.
#[must_use]
pub fn new_token() -> String {
    use rand::RngCore as _;
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_64_hex_characters_and_new_each_time() {
        let token = new_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(new_token(), token);
    }
}
