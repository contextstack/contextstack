//! Stream format v1: magic, 32-byte salt, then 1 MiB AES-256-GCM records.
//! The key is derived per archive. Record numbers are nonces, and a short
//! authenticated record (possibly empty) marks EOF.
use ring::{aead, hmac};
use std::io::{self, Read, Write};

const MAGIC: &[u8] = b"CSBACKUP\x01";
const CHUNK: usize = 1024 * 1024;
const SALT: usize = 32;
const TAG: usize = 16;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "backup authentication failed")
}

fn stream(
    input: &mut impl Read,
    output: &mut impl Write,
    master: &[u8; 32],
    encrypt: bool,
) -> io::Result<()> {
    let mut header = [0; MAGIC.len() + SALT];
    if encrypt {
        header[..MAGIC.len()].copy_from_slice(MAGIC);
        getrandom::fill(&mut header[MAGIC.len()..])
            .map_err(|_| io::Error::other("backup randomness unavailable"))?;
        output.write_all(&header)?;
    } else {
        input.read_exact(&mut header)?;
        if !header.starts_with(MAGIC) {
            return Err(invalid());
        }
    }
    let derived = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, master), &header);
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, derived.as_ref()).map_err(|_| invalid())?,
    );
    let limit = if encrypt { CHUNK } else { CHUNK + TAG };
    let mut buffer = Vec::with_capacity(CHUNK + TAG);
    for counter in 0u32.. {
        buffer.clear();
        input.take(limit as u64).read_to_end(&mut buffer)?;
        let final_record = buffer.len() < limit;
        let mut nonce = [0; 12];
        nonce[8..].copy_from_slice(&counter.to_be_bytes());
        let nonce = aead::Nonce::assume_unique_for_key(nonce);
        let aad = aead::Aad::from(header);
        if encrypt {
            key.seal_in_place_append_tag(nonce, aad, &mut buffer)
                .map_err(|_| invalid())?;
            output.write_all(&buffer)?;
        } else {
            let plaintext = key
                .open_in_place(nonce, aad, &mut buffer)
                .map_err(|_| invalid())?;
            output.write_all(plaintext)?;
        }
        if final_record {
            return output.flush();
        }
        if counter == u32::MAX {
            return Err(io::Error::other("backup exceeds maximum archive size"));
        }
    }
    unreachable!()
}

pub(crate) fn run(mode: &str) -> io::Result<()> {
    let encrypt = match mode {
        "encrypt" => true,
        "decrypt" => false,
        _ => return Err(io::Error::other("expected encrypt or decrypt")),
    };
    let mut input = io::stdin().lock();
    let mut key = [0; 32];
    input.read_exact(&mut key)?;
    stream(&mut input, &mut io::stdout().lock(), &key, encrypt)
}
