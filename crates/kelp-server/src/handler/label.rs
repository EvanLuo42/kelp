use kelp_core::ClNumber;
use kelp_meta::Command;
use kelp_meta::record::Label;
use kelp_proto::rpc::LabelInfo;
use kelp_proto::{ErrorKind, ErrorReply, Reply};

use super::{Access, Response, meta_error, not_found, parse_path};
use crate::State;

pub(super) async fn set(
    state: &State,
    access: &Access,
    name: String,
    stream: String,
    cl: Option<u64>,
    description: String,
) -> Response {
    let stream = parse_path(&stream)?;
    access.require_stream(&stream)?;
    let cl = {
        let reader = state.reader()?;
        may_change(access, reader.label(&name).map_err(meta_error)?)?;
        match cl {
            Some(cl) => ClNumber(cl),
            None => {
                reader
                    .stream(&stream)
                    .map_err(meta_error)?
                    .ok_or_else(|| not_found(format!("stream {stream}")))?
                    .head
            }
        }
    };
    let cmd = Command::SetLabel {
        name,
        target: Some((stream, cl)),
        description,
        by: access.name().to_owned(),
        at: state.clock.now(),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) async fn delete(state: &State, access: &Access, name: String) -> Response {
    let existing = state.reader()?.label(&name).map_err(meta_error)?;
    if existing.is_none() {
        return Err(not_found(format!("label {name}")));
    }
    may_change(access, existing)?;
    let cmd = Command::SetLabel {
        name,
        target: None,
        description: String::new(),
        by: access.name().to_owned(),
        at: state.clock.now(),
    };
    state.commit(access.name(), cmd).await?;
    Ok(Reply::Ok)
}

pub(super) fn get(state: &State, access: &Access, name: String) -> Response {
    match state.reader()?.label(&name).map_err(meta_error)? {
        Some(label) if access.sees(&label.stream) => Ok(Reply::Label(info(label))),
        _ => Err(not_found(format!("label {name}"))),
    }
}

pub(super) fn list(state: &State, access: &Access) -> Response {
    let labels = state.reader()?.labels().map_err(meta_error)?;
    Ok(Reply::Labels(
        labels
            .into_iter()
            .filter(|l| access.sees(&l.stream))
            .map(info)
            .collect(),
    ))
}

/// Checks that the user may move or delete `existing`.
fn may_change(access: &Access, existing: Option<Label>) -> Result<(), ErrorReply> {
    match existing {
        Some(label)
            if !access.administers(&label.stream)
                && !label.owner.eq_ignore_ascii_case(access.name()) =>
        {
            Err(ErrorReply::new(
                ErrorKind::Forbidden,
                format!("label {} belongs to {}", label.name, label.owner),
            ))
        }
        _ => Ok(()),
    }
}

fn info(label: Label) -> LabelInfo {
    LabelInfo {
        name: label.name,
        stream: label.stream.to_string(),
        cl: label.cl.0,
        description: label.description,
        owner: label.owner,
        at: label.at,
    }
}
