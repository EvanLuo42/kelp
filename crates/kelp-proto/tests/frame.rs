use kelp_proto::frame::{Flags, FrameHeader, MAX_PAYLOAD};
use kelp_proto::{Error, MsgType};

#[test]
fn header_round_trip() {
    let h = FrameHeader {
        flags: Flags::END_OF_STREAM | Flags::RAW,
        msg_type: MsgType::Chunk,
        request_id: 0xdead_beef,
        payload_len: 1 << 20,
    };
    let decoded = FrameHeader::decode(&h.encode()).unwrap();
    assert_eq!(decoded, h);
    assert!(decoded.flags.contains(Flags::RAW));
    assert!(!decoded.flags.contains(Flags::COMPRESSED));
}

#[test]
fn rejects_oversized_payload() {
    let header = FrameHeader {
        flags: Flags::default(),
        msg_type: MsgType::Hello,
        request_id: 1,
        payload_len: 0,
    };
    let mut b = header.encode();
    b[12..16].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
    assert!(matches!(
        FrameHeader::decode(&b),
        Err(Error::PayloadTooLarge(_))
    ));
}

#[test]
fn rejects_unknown_message_type() {
    let header = FrameHeader {
        flags: Flags::default(),
        msg_type: MsgType::Hello,
        request_id: 0,
        payload_len: 0,
    };
    let mut b = header.encode();
    b[4..6].copy_from_slice(&999u16.to_le_bytes());
    assert!(matches!(
        FrameHeader::decode(&b),
        Err(Error::UnknownMessage(999))
    ));
}
