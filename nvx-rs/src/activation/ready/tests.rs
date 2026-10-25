//! Tests of the canonical encoding of what Ready reports about an activation. The tests build its
//! bytes from the encoding's definition, never with the encoder that they check.

use proptest::prelude::*;

use super::*;
use crate::tests::{ACTIVATION_V1, of_kind};

/// The names of the vectors of what Ready reports, in order: those of the records whose
/// activations they report.
const READY: [&str; 5] = [
    "COLD",
    "CLONE",
    "SCRATCH_AGGREGATE",
    "GPT_DIRECTORY",
    "LARGEST",
];

/// Returns the identity of the tests: the bytes 00 through 1f.
fn identity() -> ConfigurationIdentity {
    ConfigurationIdentity::from_bytes(std::array::from_fn(|index| u8::try_from(index).unwrap()))
}

/// Returns the transaction ID of the tests: the bytes a0 through af.
fn transaction() -> TransactionId {
    TransactionId::from_bytes(std::array::from_fn(|index| {
        0xa0 + u8::try_from(index).unwrap()
    }))
}

#[test]
fn the_lengths_keep_their_values() {
    assert_eq!(
        (ReadyActivation::COLD_LEN, ReadyActivation::CLONE_LEN),
        (33, 49)
    );
}

#[test]
fn a_cold_boot_reports_its_identity_then_a_zero() {
    let ready = ReadyActivation {
        identity: identity(),
        transaction: None,
    };
    let bytes = [&identity().as_bytes()[..], &[0]].concat();
    assert_eq!(ready.encode(), bytes);
    assert_eq!(bytes.len(), ReadyActivation::COLD_LEN);
    assert_eq!(ReadyActivation::decode(&bytes), Ok(ready));
}

#[test]
fn a_clone_reports_its_identity_then_a_one_and_its_transaction() {
    let ready = ReadyActivation {
        identity: identity(),
        transaction: Some(transaction()),
    };
    let bytes = [&identity().as_bytes()[..], &[1], transaction().as_bytes()].concat();
    assert_eq!(ready.encode(), bytes);
    assert_eq!(bytes.len(), ReadyActivation::CLONE_LEN);
    assert_eq!(ReadyActivation::decode(&bytes), Ok(ready));
}

#[test]
fn every_other_form_is_refused() {
    for length in 0..=ConfigurationIdentity::LEN {
        assert_eq!(
            ReadyActivation::decode(&identity().as_bytes()[..length]),
            Err(ActivationError::Ready),
            "{length} bytes"
        );
    }
    for flag in u8::MIN..=u8::MAX {
        for tail in 0..=TransactionId::LEN + 1 {
            let bytes = [
                &identity().as_bytes()[..],
                &[flag],
                &transaction().as_bytes().repeat(2)[..tail],
            ]
            .concat();
            let decoded = ReadyActivation::decode(&bytes);
            match (flag, tail) {
                (0, 0) | (1, TransactionId::LEN) => assert!(decoded.is_ok()),
                _ => assert_eq!(
                    decoded,
                    Err(ActivationError::Ready),
                    "flag {flag}, then {tail} bytes"
                ),
            }
        }
    }
}

#[test]
fn ready_activations_match_their_vectors() {
    for vector in of_kind(ACTIVATION_V1, "READY", &READY) {
        let mut values = vector.values();
        let identity = ConfigurationIdentity::from_bytes(values.bytes().try_into().unwrap());
        let transaction =
            values.optional(|values| TransactionId::from_bytes(values.bytes().try_into().unwrap()));
        drop(values);
        let ready = ReadyActivation {
            identity,
            transaction,
        };
        assert_eq!(ready.encode(), vector.bytes, "{}", vector.name);
        assert_eq!(
            ReadyActivation::decode(&vector.bytes),
            Ok(ready),
            "{}",
            vector.name
        );
    }
}

#[test]
fn invalid_ready_activations_are_refused() {
    let names = [
        "EMPTY",
        "IDENTITY_ONLY",
        "FLAG_2",
        "COLD_WITH_TRANSACTION",
        "COLD_WITH_TRAILING_BYTE",
        "CLONE_WITHOUT_TRANSACTION",
        "SHORT_TRANSACTION",
        "LONG_TRANSACTION",
    ];
    for vector in of_kind(ACTIVATION_V1, "INVALID_READY", &names) {
        assert_eq!(
            ReadyActivation::decode(&vector.bytes),
            Err(ActivationError::Ready),
            "{}",
            vector.name
        );
    }
}

#[test]
fn the_refusal_names_the_rule() {
    assert_eq!(
        ActivationError::Ready.to_string(),
        "what Ready reports is not an identity followed by 0, or by 1 and a transaction ID"
    );
}

/// Returns bytes that a decoder may meet: arbitrary bytes, and an identity followed by a byte
/// from 0 to 2 and up to 17 more bytes.
fn inputs() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..64),
        (
            any::<[u8; 32]>(),
            0_u8..=2,
            prop::collection::vec(any::<u8>(), 0..=17)
        )
            .prop_map(|(identity, flag, tail)| [&identity[..], &[flag], &tail].concat()),
    ]
}

proptest! {
    /// Every activation encodes to bytes that decode back to it, while every prefix of them, and
    /// every longer form, is refused.
    #[test]
    fn ready_activations_round_trip(
        identity in any::<[u8; 32]>(),
        transaction in any::<Option<[u8; 16]>>(),
        cut in any::<prop::sample::Index>(),
        extra in any::<u8>(),
    ) {
        let ready = ReadyActivation {
            identity: ConfigurationIdentity::from_bytes(identity),
            transaction: transaction.map(TransactionId::from_bytes),
        };
        let mut bytes = ready.encode();
        prop_assert_eq!(ReadyActivation::decode(&bytes), Ok(ready));
        prop_assert!(ReadyActivation::decode(&bytes[..cut.index(bytes.len())]).is_err());
        bytes.push(extra);
        prop_assert!(ReadyActivation::decode(&bytes).is_err());
    }

    /// The decoder returns, never panics, on any bytes, and accepts only the one encoding of the
    /// activation that it returns.
    #[test]
    fn the_decoder_accepts_only_canonical_encodings(bytes in inputs()) {
        if let Ok(ready) = ReadyActivation::decode(&bytes) {
            prop_assert_eq!(ready.encode(), bytes);
        }
    }
}
