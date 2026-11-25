use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub struct Crypto {
    key: [u8; 32],
}
impl Crypto {
    pub fn from_env() -> Result<Self, String> {
        Self::from_base64(
            &std::env::var("PROXY_ENCRYPTION_KEY")
                .map_err(|_| "PROXY_ENCRYPTION_KEY is required when proxy manager is enabled")?,
        )
    }
    pub fn from_base64(value: &str) -> Result<Self, String> {
        let decoded = STANDARD
            .decode(value)
            .map_err(|_| "invalid Base64 proxy encryption key")?;
        Ok(Self {
            key: decoded
                .try_into()
                .map_err(|_| "proxy encryption key must decode to exactly 32 bytes")?,
        })
    }
    fn derive(&self, label: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&self.key).expect("HMAC key");
        mac.update(label);
        mac.finalize().into_bytes().into()
    }
    pub fn fingerprint(&self, data: &[u8]) -> String {
        let key = self.derive(b"raycast2api:proxy:fingerprint:v1");
        let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&key).expect("HMAC key");
        mac.update(data);
        hex::encode(mac.finalize().into_bytes())
    }
    pub fn seal(&self, record: &str, bytes: &[u8]) -> Result<String, String> {
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).map_err(|_| "random nonce generation failed")?;
        let cipher = Aes256Gcm::new_from_slice(&self.derive(b"raycast2api:proxy:encryption:v1"))
            .expect("AES key");
        let aad = format!("proxy:v1:{record}");
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: bytes,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| "proxy encryption failed")?;
        let mut envelope = vec![1u8];
        envelope.extend_from_slice(&self.derive(b"raycast2api:proxy:key-id:v1")[..8]);
        envelope.extend_from_slice(&nonce);
        envelope.extend(encrypted);
        Ok(STANDARD.encode(envelope))
    }
    pub fn open(&self, record: &str, value: &str) -> Result<Vec<u8>, String> {
        let value = STANDARD
            .decode(value)
            .map_err(|_| "invalid proxy ciphertext")?;
        if value.len() < 37
            || value[0] != 1
            || value[1..9] != self.derive(b"raycast2api:proxy:key-id:v1")[..8]
        {
            return Err("unsupported proxy ciphertext envelope".into());
        }
        let cipher = Aes256Gcm::new_from_slice(&self.derive(b"raycast2api:proxy:encryption:v1"))
            .expect("AES key");
        let aad = format!("proxy:v1:{record}");
        cipher
            .decrypt(
                Nonce::from_slice(&value[9..21]),
                Payload {
                    msg: &value[21..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| "proxy ciphertext authentication failed; check encryption key".into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ciphertext_is_random_authenticated_and_record_bound() {
        let c = Crypto::from_base64(&STANDARD.encode([7u8; 32])).unwrap();
        let a = c.seal("subscription:a", b"secret").unwrap();
        assert_ne!(a, c.seal("subscription:a", b"secret").unwrap());
        assert_eq!(c.open("subscription:a", &a).unwrap(), b"secret");
        assert!(c.open("subscription:b", &a).is_err());
        let wrong = Crypto::from_base64(&STANDARD.encode([8u8; 32])).unwrap();
        assert!(wrong.open("subscription:a", &a).is_err());
    }
}
