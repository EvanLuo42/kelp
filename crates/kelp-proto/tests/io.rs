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
async fn messages_larger_than_a_frame_span_several() {
    let (mut client, mut server) = tokio::io::duplex(1 << 20);
    let request = Request::Filelog {
        path: format!("//game/main/{}", "a".repeat(40 << 20)),
    };
    let writer = tokio::spawn(async move {
        write_message(&mut client, MsgType::Request, &request)
            .await
            .unwrap();
        request
    });
    let frame = expect_frame(&mut server).await.unwrap();
    let request = writer.await.unwrap();
    assert_eq!(frame.decode::<Request>().unwrap(), request);
    assert!(read_frame(&mut server).await.unwrap().is_none());
}

#[tokio::test]
async fn a_limit_refuses_joined_messages_beyond_it() {
    let (mut client, mut server) = tokio::io::duplex(1 << 20);
    let request = Request::Filelog {
        path: format!("//game/main/{}", "a".repeat(20 << 20)),
    };
    tokio::spawn(async move {
        let _ = write_message(&mut client, MsgType::Request, &request).await;
    });
    assert!(matches!(
        kelp_proto::io::read_frame_within(&mut server, kelp_proto::MAX_PAYLOAD).await,
        Err(Error::PayloadTooLarge(_))
    ));
}
