use super::*;

#[test]
fn values_round_trip_in_their_one_encoding() {
    let mut writer = Writer::default();
    writer.u8(0xab);
    writer.u16(0x0102);
    writer.u32(0x0304_0506);
    writer.u64(0x0708_090a_0b0c_0d0e);
    writer.bool(true);
    writer.fixed(&[9, 8]);
    writer.text("héllo");
    writer.option(Some(&7u32), |writer, value| writer.u32(*value));
    writer.option(None::<&u32>, |writer, value| writer.u32(*value));
    writer.list(&[1u8, 2], |writer, item| writer.u8(*item));
    let bytes = writer.finish();
    assert_eq!(
        bytes,
        [
            0xab, 0x02, 0x01, 0x06, 0x05, 0x04, 0x03, 0x0e, 0x0d, 0x0c, 0x0b, 0x0a, 0x09, 0x08,
            0x07, 1, 9, 8, 6, 0, 0, 0, b'h', 0xc3, 0xa9, b'l', b'l', b'o', 1, 7, 0, 0, 0, 0, 2, 0,
            0, 0, 1, 2
        ]
    );
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.u8("a").unwrap(), 0xab);
    assert_eq!(reader.u16("b").unwrap(), 0x0102);
    assert_eq!(reader.u32("c").unwrap(), 0x0304_0506);
    assert_eq!(reader.u64("d").unwrap(), 0x0708_090a_0b0c_0d0e);
    assert!(reader.bool("e").unwrap());
    assert_eq!(reader.fixed::<2>("f").unwrap(), [9, 8]);
    assert_eq!(reader.text(6, "g").unwrap(), "héllo");
    assert_eq!(
        reader.option("h", |reader| reader.u32("h")).unwrap(),
        Some(7)
    );
    assert_eq!(reader.option("i", |reader| reader.u32("i")).unwrap(), None);
    assert_eq!(
        reader.list(2, "j", |reader| reader.u8("j")).unwrap(),
        [1, 2]
    );
    reader.finish().unwrap();
}

#[test]
fn other_encodings_are_refused() {
    let refused = |bytes: &[u8], read: fn(&mut Reader<'_>) -> Result<(), CanonicalError>| {
        let mut reader = Reader::new(bytes);
        read(&mut reader)
            .and_then(|()| reader.finish())
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        refused(&[2], |reader| reader.bool("flag").map(drop)),
        "flag is 2, not a boolean"
    );
    assert_eq!(
        refused(&[2, 0], |reader| reader
            .option("value", |reader| reader.u8("value"))
            .map(drop)),
        "value has the presence tag 2, not 0 or 1"
    );
    assert_eq!(
        refused(&[3, 0, 0, 0, b'a', b'b', b'c'], |reader| reader
            .text(2, "name")
            .map(drop)),
        "name holds 3 bytes, more than 2"
    );
    assert_eq!(
        refused(&[2, 0, 0, 0, 0xff, 0xfe], |reader| reader
            .text(2, "name")
            .map(drop)),
        "name is not UTF-8"
    );
    assert_eq!(
        refused(&[2, 0, 0, 0, b'a'], |reader| reader
            .text(2, "name")
            .map(drop)),
        "name is truncated"
    );
    assert_eq!(
        refused(&[0xff, 0xff, 0, 0], |reader| {
            reader
                .list(usize::MAX, "items", |reader| reader.u8("item"))
                .map(drop)
        }),
        "items is truncated"
    );
    assert_eq!(
        refused(&[3, 0, 0, 0], |reader| {
            reader
                .list(2, "items", |reader| reader.u8("item"))
                .map(drop)
        }),
        "items holds 3 items, more than 2"
    );
    assert_eq!(
        refused(&[1, 0, 9], |reader| reader.u16("number").map(drop)),
        "a byte follows the value"
    );
    assert_eq!(
        refused(&[1, 0, 9, 9], |reader| reader.u16("number").map(drop)),
        "2 bytes follow the value"
    );
    assert_eq!(
        refused(&[1], |reader| reader.u32("number").map(drop)),
        "number is truncated"
    );
}
