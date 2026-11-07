use super::*;

/// Returns every field of `data`, or the first error.
fn fields<'a>(
    data: &'a [u8],
    known: &[u32],
    repeated: &[u32],
) -> Result<Vec<Field<'a>>, WireError> {
    let mut message = Message::new(data, known, repeated);
    let mut fields = Vec::new();
    while let Some(field) = message.next_field()? {
        fields.push(field);
    }
    Ok(fields)
}

#[test]
fn each_wire_type_reads_its_value() {
    let data = [
        0x08, 0x96, 0x01, // 1: varint 150
        0x11, 1, 2, 3, 4, 5, 6, 7, 8, // 2: fixed64
        0x1a, 0x03, b'a', b'b', b'c', // 3: bytes "abc"
        0x25, 1, 0, 0, 0x80, // 4: fixed32
    ];
    let read = fields(&data, &[1, 2, 3, 4], &[]).unwrap();
    assert_eq!(
        read,
        [
            Field {
                number: 1,
                value: Value::Varint(150)
            },
            Field {
                number: 2,
                value: Value::Fixed64(0x0807_0605_0403_0201)
            },
            Field {
                number: 3,
                value: Value::Bytes(b"abc")
            },
            Field {
                number: 4,
                value: Value::Fixed32(0x8000_0001)
            },
        ]
    );
    assert_eq!(read[0].uint32().unwrap(), 150);
    assert_eq!(read[2].text().unwrap(), "abc");
    assert_eq!(
        read[0].bytes().unwrap_err(),
        WireError::Type {
            number: 1,
            expected: "bytes"
        }
    );
    assert_eq!(
        read[2].uint64().unwrap_err(),
        WireError::Type {
            number: 3,
            expected: "an integer"
        }
    );
}

#[test]
fn varints_hold_64_bits_in_at_most_ten_bytes() {
    let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
    assert_eq!(read_varint(&mut &max[..]).unwrap(), u64::MAX);
    // A tenth byte with more than the 64th bit, and an eleventh byte, overflow.
    let mut over = max;
    over[9] = 0x02;
    assert_eq!(read_varint(&mut &over[..]), Err(WireError::Varint));
    let long = [0x80; 11];
    assert_eq!(read_varint(&mut &long[..]), Err(WireError::Varint));
    // A varint whose last byte is missing is truncated.
    assert_eq!(
        read_varint(&mut &[0x80, 0x80][..]),
        Err(WireError::Truncated)
    );
    assert_eq!(read_varint(&mut &[][..]), Err(WireError::Truncated));
    // A longer encoding than needed still reads.
    assert_eq!(read_varint(&mut &[0x81, 0x00][..]).unwrap(), 1);
}

#[test]
fn signed_integers_are_twos_complement() {
    let minus_one = [
        0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
    ];
    let read = fields(&minus_one, &[1], &[]).unwrap();
    assert_eq!(read[0].int64().unwrap(), -1);
    assert_eq!(read[0].int32().unwrap(), -1);
    assert!(read[0].uint32().is_err());
    let large = [0x08, 0x80, 0x80, 0x80, 0x80, 0x10];
    assert!(fields(&large, &[1], &[]).unwrap()[0].int32().is_err());
}

#[test]
fn booleans_and_packed_lists_are_strict() {
    let read = fields(&[0x08, 0x01, 0x10, 0x02], &[1, 2], &[]).unwrap();
    assert!(read[0].boolean().unwrap());
    assert_eq!(
        read[1].boolean().unwrap_err().to_string(),
        "holds field 2, which is 2, not a boolean"
    );
    let packed = [0x0a, 0x04, 0x01, 0x80, 0x01, 0x00];
    assert_eq!(
        fields(&packed, &[1], &[]).unwrap()[0]
            .packed_uint32()
            .unwrap(),
        [1, 128, 0]
    );
    let truncated = [0x0a, 0x02, 0x01, 0x80];
    assert_eq!(
        fields(&truncated, &[1], &[]).unwrap()[0].packed_uint32(),
        Err(WireError::Truncated)
    );
    let wide = [0x0a, 0x05, 0x80, 0x80, 0x80, 0x80, 0x10];
    assert!(
        fields(&wide, &[1], &[]).unwrap()[0]
            .packed_uint32()
            .is_err()
    );
}

#[test]
fn malformed_messages_are_refused() {
    // A length past the end, a fixed value cut short, and a key without its value.
    assert_eq!(
        fields(&[0x0a, 0x05, b'a'], &[1], &[]),
        Err(WireError::Truncated)
    );
    assert_eq!(
        fields(&[0x09, 1, 2, 3], &[1], &[]),
        Err(WireError::Truncated)
    );
    assert_eq!(fields(&[0x08], &[1], &[]), Err(WireError::Truncated));
    // Groups and the wire types that do not exist.
    for (key, wire_type) in [(0x0b, 3), (0x0c, 4), (0x0e, 6), (0x0f, 7)] {
        assert_eq!(
            fields(&[key], &[1], &[]),
            Err(WireError::WireType(wire_type))
        );
    }
    // Field 0 and a number beyond 2^29 - 1.
    assert_eq!(
        fields(&[0x00, 0x00], &[1], &[]),
        Err(WireError::FieldNumber(0))
    );
    let beyond = [0x80, 0x80, 0x80, 0x80, 0x10, 0x00];
    assert_eq!(
        fields(&beyond, &[1], &[]),
        Err(WireError::FieldNumber(1 << 29))
    );
}

#[test]
fn messages_refuse_unknown_and_repeated_fields() {
    assert_eq!(fields(&[0x10, 0x01], &[1], &[]), Err(WireError::Unknown(2)));
    // A field number beyond any that a message type knows is unknown too.
    assert_eq!(
        fields(&[0xc0, 0x0c, 0x01], &[1], &[]),
        Err(WireError::Unknown(200))
    );
    assert_eq!(
        fields(&[0x08, 0x01, 0x08, 0x02], &[1], &[]),
        Err(WireError::Repeated(1))
    );
    assert_eq!(
        fields(&[0x0a, 0x00, 0x0a, 0x00], &[], &[1]).unwrap().len(),
        2
    );
    assert_eq!(fields(&[], &[1], &[]).unwrap(), []);
}
