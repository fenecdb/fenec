//! SCRAM-SHA-256, server side (RFC 5802 + RFC 7677).
//!
//! Why it is needed: in PostgreSQL's `AuthenticationCleartextPassword` flow
//! the password travels the wire in plain text. Without TLS that makes the
//! password readable on the network. With SCRAM only nonces and HMAC proofs
//! travel; the password is never sent and a recording cannot be replayed
//! (a fresh nonce per session).
//!
//! SASLprep is the identity transform for ASCII passwords; this server takes
//! the password as UTF-8 bytes (which gives exactly the same result as libpq
//! for ASCII passwords).

use crate::crypto::*;

const ITERS: u32 = 4096;

/// The password verifier kept on the server. The password goes through
/// PBKDF2 and only `stored_key`/`server_key` are stored.
pub struct Verifier {
    salt: Vec<u8>,
    iters: u32,
    stored_key: [u8; SHA256_LEN],
    server_key: [u8; SHA256_LEN],
}

impl Verifier {
    pub fn new(password: &str) -> Verifier {
        Verifier::with_salt(password, random_bytes(16), ITERS)
    }

    pub fn with_salt(password: &str, salt: Vec<u8>, iters: u32) -> Verifier {
        let salted = pbkdf2_sha256(password.as_bytes(), &salt, iters);
        let client_key = hmac_sha256(&salted, b"Client Key");
        Verifier {
            salt,
            iters,
            stored_key: sha256(&client_key),
            server_key: hmac_sha256(&salted, b"Server Key"),
        }
    }
}

/// A single-session SCRAM handshake.
pub struct Exchange<'a> {
    v: &'a Verifier,
    gs2_header: String,
    client_first_bare: String,
    server_first: String,
    nonce: String,
}

pub type ScramResult<T> = std::result::Result<T, String>;

fn attrs(s: &str) -> Vec<(char, &str)> {
    s.split(',')
        .filter_map(|kv| {
            let mut it = kv.chars();
            let k = it.next()?;
            let rest = kv.get(kv.char_indices().nth(1)?.0..)?;
            rest.strip_prefix('=').map(|v| (k, v))
        })
        .collect()
}

impl<'a> Exchange<'a> {
    pub fn new(v: &'a Verifier) -> Exchange<'a> {
        Exchange {
            v,
            gs2_header: String::new(),
            client_first_bare: String::new(),
            server_first: String::new(),
            nonce: String::new(),
        }
    }

    /// `client-first-message` -> `server-first-message`.
    pub fn client_first(&mut self, msg: &[u8]) -> ScramResult<String> {
        let msg = std::str::from_utf8(msg).map_err(|_| "the SCRAM message is not UTF-8".to_string())?;

        // gs2-header: "n,," | "y,," | "p=<type>,," ; an authzid is allowed: "n,a=x,"
        let mut parts = msg.splitn(3, ',');
        let cb = parts.next().unwrap_or("");
        let authzid = parts.next().unwrap_or("");
        let bare = parts
            .next()
            .ok_or_else(|| "malformed SCRAM client-first".to_string())?;
        if cb.starts_with('p') {
            // Channel binding requires TLS; this server has no TLS.
            return Err("channel binding is not supported (no TLS)".into());
        }
        if cb != "n" && cb != "y" {
            return Err("malformed SCRAM gs2 header".into());
        }
        self.gs2_header = format!("{cb},{authzid},");
        self.client_first_bare = bare.to_string();

        let cnonce = attrs(bare)
            .into_iter()
            .find(|(k, _)| *k == 'r')
            .map(|(_, v)| v.to_string())
            .ok_or_else(|| "no SCRAM client nonce".to_string())?;
        if cnonce.is_empty() {
            return Err("the SCRAM client nonce is empty".into());
        }
        self.nonce = format!("{cnonce}{}", nonce(18));
        self.server_first = format!(
            "r={},s={},i={}",
            self.nonce,
            b64_encode(&self.v.salt),
            self.v.iters
        );
        Ok(self.server_first.clone())
    }

    /// `client-final-message` -> `server-final-message`. Errors on a bad proof.
    pub fn client_final(&mut self, msg: &[u8]) -> ScramResult<String> {
        let msg = std::str::from_utf8(msg).map_err(|_| "the SCRAM message is not UTF-8".to_string())?;
        let without_proof = msg
            .rsplit_once(",p=")
            .map(|(head, _)| head)
            .ok_or_else(|| "no SCRAM proof".to_string())?;
        let a = attrs(msg);
        let get = |key: char| a.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);

        // The channel binding value: the client's gs2 header from the first message.
        let c = get('c').ok_or_else(|| "no SCRAM `c` field".to_string())?;
        if b64_decode(c).as_deref() != Some(self.gs2_header.as_bytes()) {
            return Err("the SCRAM channel binding value does not match".into());
        }
        // The nonce must match the server's: this prevents replay.
        if get('r') != Some(self.nonce.as_str()) {
            return Err("the SCRAM nonce does not match".into());
        }
        let proof = get('p')
            .and_then(b64_decode)
            .ok_or_else(|| "the SCRAM proof could not be decoded".to_string())?;
        if proof.len() != SHA256_LEN {
            return Err("wrong SCRAM proof length".into());
        }

        let auth_message = format!(
            "{},{},{}",
            self.client_first_bare, self.server_first, without_proof
        );
        let client_sig = hmac_sha256(&self.v.stored_key, auth_message.as_bytes());
        let mut client_key = [0u8; SHA256_LEN];
        for i in 0..SHA256_LEN {
            client_key[i] = proof[i] ^ client_sig[i];
        }
        if !ct_eq(&sha256(&client_key), &self.v.stored_key) {
            return Err("password verification failed".into());
        }
        let server_sig = hmac_sha256(&self.v.server_key, auth_message.as_bytes());
        Ok(format!("v={}", b64_encode(&server_sig)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 7677 example flow: builds the client side by hand and checks
    /// that the server produces the same values.
    #[test]
    fn rfc7677_exchange() {
        let salt = b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let v = Verifier::with_salt("pencil", salt, 4096);
        let mut ex = Exchange::new(&v);
        let server_first = ex
            .client_first(b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO")
            .unwrap();
        // The server nonce is random; compute the client proof with that nonce.
        let snonce = server_first
            .strip_prefix("r=")
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .to_string();
        assert!(snonce.starts_with("rOprNGfwEbeRWgbNEkqO"));
        assert!(server_first.contains("s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"));

        let without_proof = format!("c=biws,r={snonce}");
        let auth = format!("n=user,r=rOprNGfwEbeRWgbNEkqO,{server_first},{without_proof}");
        let salted = pbkdf2_sha256(b"pencil", &v.salt, 4096);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let client_sig = hmac_sha256(&sha256(&client_key), auth.as_bytes());
        let mut proof = [0u8; SHA256_LEN];
        for i in 0..SHA256_LEN {
            proof[i] = client_key[i] ^ client_sig[i];
        }
        let final_msg = format!("{without_proof},p={}", b64_encode(&proof));
        let server_final = ex.client_final(final_msg.as_bytes()).unwrap();

        let server_key = hmac_sha256(&salted, b"Server Key");
        let expected = format!("v={}", b64_encode(&hmac_sha256(&server_key, auth.as_bytes())));
        assert_eq!(server_final, expected);
    }

    #[test]
    fn wrong_password_fails() {
        let salt = b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let v = Verifier::with_salt("right", salt.clone(), 4096);
        let mut ex = Exchange::new(&v);
        let server_first = ex.client_first(b"n,,n=user,r=abcdefghijkl").unwrap();
        let snonce = server_first.strip_prefix("r=").unwrap().split(',').next().unwrap();
        let without_proof = format!("c=biws,r={snonce}");
        let auth = format!("n=user,r=abcdefghijkl,{server_first},{without_proof}");
        let salted = pbkdf2_sha256(b"wrong", &salt, 4096);
        let ck = hmac_sha256(&salted, b"Client Key");
        let cs = hmac_sha256(&sha256(&ck), auth.as_bytes());
        let mut proof = [0u8; SHA256_LEN];
        for i in 0..SHA256_LEN {
            proof[i] = ck[i] ^ cs[i];
        }
        let msg = format!("{without_proof},p={}", b64_encode(&proof));
        assert!(ex.client_final(msg.as_bytes()).is_err());
    }

    #[test]
    fn replayed_nonce_fails() {
        let v = Verifier::new("secret");
        let mut ex = Exchange::new(&v);
        ex.client_first(b"n,,n=,r=clientnonce").unwrap();
        // Sending the client's nonce instead of the server's is rejected.
        let msg = format!("c=biws,r=clientnonce,p={}", b64_encode(&[0u8; 32]));
        let e = ex.client_final(msg.as_bytes()).unwrap_err();
        assert!(e.contains("nonce"));
    }

    #[test]
    fn channel_binding_required_is_rejected() {
        let v = Verifier::new("secret");
        let mut ex = Exchange::new(&v);
        assert!(ex
            .client_first(b"p=tls-server-end-point,,n=,r=abc")
            .is_err());
    }

    #[test]
    fn tampered_gs2_is_rejected() {
        let v = Verifier::new("secret");
        let mut ex = Exchange::new(&v);
        let sf = ex.client_first(b"y,,n=,r=abcdefghijkl").unwrap();
        let snonce = sf.strip_prefix("r=").unwrap().split(',').next().unwrap();
        // "y,," is expected; if the client sends "n,," (biws) it is dropped.
        let msg = format!("c=biws,r={snonce},p={}", b64_encode(&[0u8; 32]));
        assert!(ex.client_final(msg.as_bytes()).unwrap_err().contains("channel"));
    }
}
