//! What the signatures need, without new crates: SHA-256 from CommonCrypto,
//! base64, and checking an ECDSA P-256 signature with Security.framework
//! (lfm3-vl-server docs/native-app.md §14.5, U9). The unsafe calls are all here.

use std::ffi::c_void;
use std::ptr;

type CFTypeRef = *const c_void;
type CFIndex = std::ffi::c_long;

#[repr(C)]
struct CFDictionaryKeyCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
}

#[repr(C)]
struct CFDictionaryValueCallBacks {
    version: CFIndex,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
    fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: CFIndex) -> CFTypeRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: CFIndex,
        key_callbacks: *const CFDictionaryKeyCallBacks,
        value_callbacks: *const CFDictionaryValueCallBacks,
    ) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecAttrKeyType: CFTypeRef;
    static kSecAttrKeyTypeECSECPrimeRandom: CFTypeRef;
    static kSecAttrKeyClass: CFTypeRef;
    static kSecAttrKeyClassPublic: CFTypeRef;
    static kSecKeyAlgorithmECDSASignatureMessageX962SHA256: CFTypeRef;
    fn SecKeyCreateWithData(key_data: CFTypeRef, attributes: CFTypeRef, error: *mut CFTypeRef) -> CFTypeRef;
    fn SecKeyVerifySignature(key: CFTypeRef, algorithm: CFTypeRef, signed_data: CFTypeRef, signature: CFTypeRef, error: *mut CFTypeRef) -> u8;
}

// CommonCrypto lives in libSystem
unsafe extern "C" {
    fn CC_SHA256(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
}

/// A Core Foundation object of ours, released when dropped.
struct Cf(CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own one reference to a live CF object
            unsafe { CFRelease(self.0) };
        }
    }
}

fn cf_data(bytes: &[u8]) -> Cf {
    // SAFETY: CFDataCreate copies len bytes from a valid slice
    Cf(unsafe { CFDataCreate(ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex) })
}

/// A CFDictionary of the CF keys and values given (retained by it).
fn cf_dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: the arrays hold count live CF objects; the callbacks retain them
    Cf(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as CFIndex,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    })
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut md = [0u8; 32];
    // SAFETY: CC_SHA256 reads len bytes and writes 32 into md
    unsafe { CC_SHA256(data.as_ptr().cast(), data.len() as u32, md.as_mut_ptr()) };
    md
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A key's number (X-VA-Key): the first 16 hex digits of the SHA-256 of its
/// public part (the 65-byte X9.63 point).
pub fn key_id(x963: &[u8]) -> String {
    hex(&sha256(x963))[..16].to_string()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Only tests write base64 (the iPhone does in use).
#[cfg(test)]
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | ((*b as u32) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The standard alphabet with padding; anything else is None.
pub fn unbase64(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for (q, quad) in s.chunks(4).enumerate() {
        let last = q == s.len() / 4 - 1;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in quad.iter().enumerate() {
            let v = if i >= 4 - pad { 0 } else { B64.iter().position(|&b| b == c)? as u32 };
            n |= v << (18 - 6 * i);
        }
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

/// Whether signature (DER, as the iPhone's SecKeyCreateSignature makes with
/// .ecdsaSignatureMessageX962SHA256) signs message with the P-256 key whose
/// public part is x963.
pub fn verify_p256(x963: &[u8], message: &[u8], signature: &[u8]) -> bool {
    if x963.len() != 65 || x963[0] != 4 || signature.is_empty() {
        return false;
    }
    // SAFETY: the statics are Security.framework's constant CFStrings
    let attrs = unsafe { cf_dict(&[(kSecAttrKeyType, kSecAttrKeyTypeECSECPrimeRandom), (kSecAttrKeyClass, kSecAttrKeyClassPublic)]) };
    let data = cf_data(x963);
    // SAFETY: data and attrs are live; a null error pointer asks for no error object
    let key = Cf(unsafe { SecKeyCreateWithData(data.0, attrs.0, ptr::null_mut()) });
    if key.0.is_null() {
        return false;
    }
    let (msg, sig) = (cf_data(message), cf_data(signature));
    // SAFETY: every argument is a live CF object
    unsafe { SecKeyVerifySignature(key.0, kSecKeyAlgorithmECDSASignatureMessageX962SHA256, msg.0, sig.0, ptr::null_mut()) != 0 }
}

/// A software P-256 key for tests: it signs as the iPhone's keys do.
#[cfg(test)]
pub mod testing {
    use super::*;

    const CF_NUMBER_SINT32: CFIndex = 3;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
        fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
        fn CFNumberCreate(allocator: CFTypeRef, the_type: CFIndex, value: *const c_void) -> CFTypeRef;
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecAttrKeySizeInBits: CFTypeRef;
        fn SecKeyCreateRandomKey(parameters: CFTypeRef, error: *mut CFTypeRef) -> CFTypeRef;
        fn SecKeyCopyPublicKey(key: CFTypeRef) -> CFTypeRef;
        fn SecKeyCopyExternalRepresentation(key: CFTypeRef, error: *mut CFTypeRef) -> CFTypeRef;
        fn SecKeyCreateSignature(key: CFTypeRef, algorithm: CFTypeRef, data: CFTypeRef, error: *mut CFTypeRef) -> CFTypeRef;
    }

    fn cf_i32(v: i32) -> Cf {
        // SAFETY: reads one i32 from a valid pointer
        Cf(unsafe { CFNumberCreate(ptr::null(), CF_NUMBER_SINT32, (&v as *const i32).cast()) })
    }

    fn bytes(data: &Cf) -> Vec<u8> {
        assert!(!data.0.is_null());
        // SAFETY: data is a live CFData; the slice is copied before it goes
        unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data.0), CFDataGetLength(data.0) as usize).to_vec() }
    }

    pub struct TestKey(Cf);

    // SAFETY: SecKey objects are immutable and thread-safe
    unsafe impl Send for TestKey {}
    unsafe impl Sync for TestKey {}

    impl TestKey {
        pub fn new() -> TestKey {
            let size = cf_i32(256);
            // SAFETY: Security.framework's constant CFStrings and a live CFNumber
            let params = unsafe { cf_dict(&[(kSecAttrKeyType, kSecAttrKeyTypeECSECPrimeRandom), (kSecAttrKeySizeInBits, size.0)]) };
            // SAFETY: params is live; the key is in memory only (not permanent)
            let key = Cf(unsafe { SecKeyCreateRandomKey(params.0, ptr::null_mut()) });
            assert!(!key.0.is_null(), "SecKeyCreateRandomKey failed");
            TestKey(key)
        }

        /// The public part as the iPhone sends it (X9.63, 65 bytes).
        pub fn public(&self) -> Vec<u8> {
            // SAFETY: the key is live; both results are released by Cf
            let public = Cf(unsafe { SecKeyCopyPublicKey(self.0.0) });
            bytes(&Cf(unsafe { SecKeyCopyExternalRepresentation(public.0, ptr::null_mut()) }))
        }

        pub fn id(&self) -> String {
            key_id(&self.public())
        }

        pub fn sign(&self, message: &[u8]) -> Vec<u8> {
            let data = cf_data(message);
            // SAFETY: the key and data are live
            bytes(&Cf(unsafe { SecKeyCreateSignature(self.0.0, kSecKeyAlgorithmECDSASignatureMessageX962SHA256, data.0, ptr::null_mut()) }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TestKey;
    use super::*;

    #[test]
    fn sha256_of_known_strings() {
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn base64_both_ways() {
        for (raw, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(raw.as_bytes()), enc);
            assert_eq!(unbase64(enc).as_deref(), Some(raw.as_bytes()), "{enc}");
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(unbase64(&base64(&all)), Some(all));
    }

    #[test]
    fn base64_refuses_what_is_not() {
        for bad in ["Zg=", "Zg", "Z===", "Zg==Zg==", "Z!==", "Zm9v\n"] {
            assert_eq!(unbase64(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_signature_checks_out_and_any_change_does_not() {
        let k = TestKey::new();
        let public = k.public();
        assert_eq!(public.len(), 65);
        assert_eq!(public[0], 4);
        let sig = k.sign(b"VA1\nPOST\n/agent/stop");
        assert!(verify_p256(&public, b"VA1\nPOST\n/agent/stop", &sig));
        assert!(!verify_p256(&public, b"VA1\nPOST\n/agent/stoq", &sig));
        let mut bent = sig.clone();
        let last = bent.len() - 1;
        bent[last] ^= 1;
        assert!(!verify_p256(&public, b"VA1\nPOST\n/agent/stop", &bent));
        // another key's signature
        assert!(!verify_p256(&TestKey::new().public(), b"VA1\nPOST\n/agent/stop", &sig));
    }

    #[test]
    fn malformed_keys_and_signatures_are_refused() {
        let k = TestKey::new();
        let sig = k.sign(b"m");
        assert!(!verify_p256(&k.public()[..64], b"m", &sig));
        assert!(!verify_p256(&[0u8; 65], b"m", &sig));
        assert!(!verify_p256(&k.public(), b"m", &[]));
        assert!(!verify_p256(&k.public(), b"m", b"not der"));
    }

    #[test]
    fn a_key_id_is_16_hex_digits_of_its_hash() {
        let k = TestKey::new();
        assert_eq!(k.id().len(), 16);
        assert_eq!(k.id(), hex(&sha256(&k.public()))[..16]);
    }
}
