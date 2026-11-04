use kelp_core::ChunkId;
use kelp_proto::io::{expect_frame, read_frame, split_chunk, write_chunk, write_message};
use kelp_proto::{Error, MsgType, Request};

#[tokio::test]
async fn frames_round_trip_over_a_stream() {
    let (mut client, mut server) = tokio::io::duplex(1024);
    let request = Request::Filelog {
        path: "//game/main/a.txt".into(),
    };
    let chunk = b"chunk bytes";
    let id = ChunkId::of(chunk);

    write_message(&mut client, MsgType::Request, &request)
        .await
        .unwrap();
    write_chunk(&mut client, &id.0.0, chunk).await.unwrap();
    drop(client);

    let frame = expect_frame(&mut server).await.unwrap();
    frame.expect(MsgType::Request).unwrap();
    assert_eq!(frame.decode::<Request>().unwrap(), request);

    let frame = expect_frame(&mut server).await.unwrap();
    assert!(matches!(
        frame.expect(MsgType::Reply),
        Err(Error::Unexpected { .. })
    ));
    let (got_id, data) = split_chunk(&frame.payload).unwrap();
    assert_eq!((got_id, data), (id.0.0, chunk.as_slice()));

    assert!(read_frame(&mut server).await.unwrap().is_none());
}

#[tokio::test]
async fn truncated_frame_is_an_error() {
    let (mut client, mut server) = tokio::io::duplex(1024);
    write_message(&mut client, MsgType::Request, &Request::ListStreams)
        .await
        .unwrap();
    let mut partial = Vec::new();
    write_message(&mut partial, MsgType::Request, &Request::ListStreams)
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut client, &partial[..10])
        .await
        .unwrap();
    drop(client);

    expect_frame(&mut server).await.unwrap();
    assert!(matches!(
        read_frame(&mut server).await,
        Err(Error::UnexpectedEof)
    ));
}

#[tokio::test]
async fn a_large_claimed_payload_that_never_arrives_is_an_error() {
    use kelp_proto::{Flags, FrameHeader, MAX_PAYLOAD};
    let (mut client, mut server) = tokio::io::duplex(1024);
    let header = FrameHeader {
        flags: Flags::RAW,
        msg_type: MsgType::Chunk,
        request_id: 0,
        payload_len: MAX_PAYLOAD,
    };
    tokio::io::AsyncWriteExt::write_all(&mut client, &header.encode())
        .await
        .unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut client, b"only a few bytes")
        .await
        .unwrap();
    drop(client);
    assert!(matches!(
        read_frame(&mut server).await,
        Err(Error::UnexpectedEof)
    ));
}

#[tokio::test]
async fn payloads_larger_than_the_first_allocation_round_trip() {
    let (mut client, mut server) = tokio::io::duplex(1 << 16);
    let data = kelp_testkit::pseudo_random(1 << 20, 7);
    let id = ChunkId::of(&data);
    let writer = tokio::spawn(async move {
        write_chunk(&mut client, &id.0.0, &data).await.unwrap();
        data
    });
    let frame = expect_frame(&mut server).await.unwrap();
    let data = writer.await.unwrap();
    let (got_id, got) = split_chunk(&frame.payload).unwrap();
    assert_eq!((got_id, got), (id.0.0, data.as_slice()));
}
