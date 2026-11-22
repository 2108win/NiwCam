//! Reproduces docs/protocol-vectors.json (protocol appendix A), the same
//! values `niwcam-protocol-test` checks on the C++ side.

use niwcam_core::crypto::*;

fn vectors() -> serde_json::Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/protocol-vectors.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("vectors file")).expect("json")
}

#[test]
fn matches_protocol_vectors() {
    let v = vectors();
    let s = |k: &str| {
        v[k].as_str()
            .unwrap_or_else(|| panic!("missing {k}"))
            .to_string()
    };

    let pc_pub = from_hex(&s("pcPublic"));
    let phone_pub = from_hex(&s("phonePublic"));
    let pc = EcdhKey::import(&pc_pub, &from_hex(&s("pcPrivate"))).unwrap();

    assert_eq!(to_hex(&sha256(&[&pc_pub])), s("commit"));
    let z = pc.agree(&phone_pub).unwrap();
    assert_eq!(to_hex(&z), s("Z"));
    let t = transcript(&s("clientId"), &s("deviceId"), &pc_pub, &phone_pub);
    assert_eq!(to_hex(&t), s("T"));
    assert_eq!(sas(&t), s("SAS"));
    let k_pair = pair_key(&z, &t);
    assert_eq!(to_hex(&k_pair), s("K_pair"));
    assert_eq!(to_hex(&pc_mac(&k_pair, &t)), s("macPc"));
    assert_eq!(to_hex(&phone_mac(&k_pair, &t)), s("macPhone"));
    let up = up_key(&z, &k_pair, &t);
    assert_eq!(to_hex(&up), s("K_up"));
    assert_eq!(to_hex(&down_key(&z, &k_pair, &t)), s("K_down"));

    // The phone's side reaches the same Z.
    let phone = EcdhKey::import(&phone_pub, &from_hex(&s("phonePrivate"))).unwrap();
    assert_eq!(phone.agree(&pc_pub).unwrap(), z);

    let plain = from_hex(&s("sealedPlaintext"));
    let sealed = seal(&up, DIR_UP, 0, &plain).unwrap();
    let mut record = vec![
        0,
        0,
        0,
        (1 + 8 + sealed.len()) as u8,
        0x02,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    record.extend_from_slice(&sealed);
    assert_eq!(to_hex(&record), s("sealedRecord"));

    assert_eq!(open(&up, DIR_UP, 0, &sealed).unwrap(), plain);
    let mut tampered = sealed.clone();
    tampered[3] ^= 1;
    assert!(open(&up, DIR_UP, 0, &tampered).is_none());
}
