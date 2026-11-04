//! Routes each request to the handlers of its area.

use super::*;

/// Sends `request` to the handler for it, as the user `access` describes.
pub(super) async fn dispatch(
    state: &Arc<State>,
    session: &Session,
    access: Access,
    request: Request,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Response {
    match request {
        Request::Login { .. }
        | Request::OidcStart
        | Request::OidcPoll { .. }
        | Request::Raft { .. } => {
            unreachable!("handled before dispatch")
        }
        r @ (Request::TotpEnroll
        | Request::TotpConfirm { .. }
        | Request::TotpDisable { .. }
        | Request::CreateUser { .. }
        | Request::IssueToken { .. }
        | Request::SetGroup { .. }
        | Request::ListGroups
        | Request::GetProtections
        | Request::SetProtections { .. }
        | Request::OidcLink { .. }
        | Request::ExpectSso { .. }
        | Request::UnlinkSso { .. }
        | Request::MergeUser { .. }
        | Request::GetUser { .. }
        | Request::ListUsers
        | Request::SetPassword { .. }
        | Request::SetDisabled { .. }
        | Request::EditProfile { .. }
        | Request::Logout) => accounts(state, session, access, r).await,
        r @ (Request::SetLabel { .. }
        | Request::DeleteLabel { .. }
        | Request::GetLabel { .. }
        | Request::ListLabels) => labels(state, access, r).await,
        r @ (Request::DeleteStream { .. }
        | Request::CreateDepot { .. }
        | Request::GetTypemap { .. }
        | Request::SetTypemap { .. }
        | Request::CreateStream { .. }
        | Request::ListStreams
        | Request::GetImports { .. }
        | Request::SetImports { .. }
        | Request::ResolveStream { .. }
        | Request::DiffTree { .. }
        | Request::GetIntegration { .. }
        | Request::CopyUp { .. }) => streams(state, access, r).await,
        r @ (Request::HasChunks { .. }
        | Request::PutChunks { .. }
        | Request::GetChunks { .. }
        | Request::PutManifests { .. }
        | Request::GetManifests { .. }
        | Request::GetContentChunks { .. }) => content(state, access, r, send, recv).await,
        r @ (Request::Submit { .. }
        | Request::Filelog { .. }
        | Request::Changes { .. }
        | Request::Describe { .. }
        | Request::AcquireLocks { .. }
        | Request::ReleaseLocks { .. }
        | Request::ListLocks { .. }
        | Request::Shelve { .. }
        | Request::GetShelf { .. }
        | Request::DeleteShelf { .. }) => changes(state, access, r).await,
        r @ (Request::ClusterStatus
        | Request::Gc { .. }
        | Request::Obliterate { .. }
        | Request::AuditLog { .. }
        | Request::VerifyAudit
        | Request::Tier
        | Request::Backup { .. }
        | Request::Fsck { .. }
        | Request::ReadLog { .. }) => admin(state, access, r).await,
        r @ (Request::PublishArtifact { .. }
        | Request::FindArtifacts { .. }
        | Request::ArtifactFiles { .. }
        | Request::GetArtifact { .. }
        | Request::PinArtifact { .. }
        | Request::DeleteArtifact { .. }
        | Request::SetArtifactRetention { .. }) => artifacts(state, access, r).await,
        r @ (Request::OpenReview { .. }
        | Request::Vote { .. }
        | Request::ReportStatus { .. }
        | Request::CloseReview { .. }
        | Request::GetReview { .. }
        | Request::ListReviews { .. }
        | Request::GetReviewRules
        | Request::SetReviewRules { .. }
        | Request::SetWebhook { .. }
        | Request::DeleteWebhook { .. }
        | Request::ListWebhooks
        | Request::Deliveries { .. }
        | Request::Redeliver { .. }
        | Request::RegisterPlugin { .. }
        | Request::DeletePlugin { .. }
        | Request::GetPlugin { .. }
        | Request::ListPlugins
        | Request::GetIssue { .. }
        | Request::LinkIssues { .. }
        | Request::IssuesIn { .. }) => governance(state, access, r).await,
    }
}

async fn accounts(
    state: &Arc<State>,
    session: &Session,
    access: Access,
    request: Request,
) -> Response {
    match request {
        Request::TotpEnroll => account::totp_enroll(&access).await,
        Request::TotpConfirm { secret, code } => {
            account::totp_confirm(state, &access, secret, code).await
        }
        Request::TotpDisable { user, code } => {
            account::totp_disable(state, &access, user, code).await
        }
        Request::CreateUser {
            name,
            password,
            admin,
            replica,
        } => account::create_user(state, &access, name, password, admin, replica).await,
        Request::IssueToken { user, ttl_days } => {
            account::issue_token(state, &access, user, ttl_days).await
        }
        Request::SetGroup { name, members } => {
            account::set_group(state, &access, name, members).await
        }
        Request::ListGroups => account::list_groups(state),
        Request::GetProtections => account::get_protections(state, &access),
        Request::SetProtections { source } => {
            account::set_protections(state, &access, source).await
        }
        Request::OidcLink { device_code } => {
            account::oidc_link(state, session, &access, device_code).await
        }
        Request::ExpectSso { user, sso_name } => {
            account::expect_sso(state, &access, user, sso_name).await
        }
        Request::UnlinkSso { user } => account::unlink_sso(state, &access, user).await,
        Request::MergeUser { from, into } => account::merge_user(state, &access, from, into).await,
        Request::GetUser { name } => account::get_user(state, &access, name),
        Request::ListUsers => account::list_users(state, &access),
        Request::Logout => account::logout(state, session, &access).await,
        Request::SetPassword {
            user,
            current,
            password,
        } => account::set_password(state, &access, user, current, password).await,
        Request::SetDisabled { user, disabled } => {
            account::set_disabled(state, &access, user, disabled).await
        }
        Request::EditProfile { user, change } => {
            account::edit_profile(state, &access, user, change).await
        }
        _ => unreachable!("dispatched by area"),
    }
}

async fn streams(state: &Arc<State>, access: Access, request: Request) -> Response {
    match request {
        Request::DeleteStream { stream: path } => stream::delete_stream(state, &access, path).await,
        Request::CreateDepot { name } => stream::create_depot(state, &access, name).await,
        Request::GetTypemap { depot } => stream::get_typemap(state, depot),
        Request::SetTypemap { depot, source } => {
            stream::set_typemap(state, &access, depot, source).await
        }
        Request::CreateStream { path, kind, parent } => {
            stream::create_stream(state, &access, path, kind, parent).await
        }
        Request::ListStreams => stream::list(state, &access),
        Request::GetImports { stream } => stream::get_imports(state, &access, stream),
        Request::SetImports { stream, imports } => {
            stream::set_imports(state, &access, stream, imports).await
        }
        Request::ResolveStream { stream, at } => stream::resolve(state, &access, stream, at),
        Request::DiffTree {
            stream,
            from,
            to,
            paths,
        } => stream::diff_tree(state, access, stream, from, to, paths).await,
        Request::GetIntegration { target, source } => {
            stream::integration(state, &access, target, source)
        }
        Request::CopyUp {
            stream,
            description,
        } => stream::copy_up(state, access, stream, description).await,
        _ => unreachable!("dispatched by area"),
    }
}

async fn content(
    state: &Arc<State>,
    access: Access,
    request: Request,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Response {
    match request {
        Request::HasChunks { ids } => content::has_chunks(state, ids).await,
        Request::PutChunks { count } => content::put_chunks(state, count, recv).await,
        Request::GetChunks { ids } => {
            check_batch(ids.len())?;
            content::require_reads_everything(&access)?;
            edge::fill_chunks(state, &ids).await?;
            content::get_chunks(state, ids, send).await
        }
        Request::PutManifests { manifests } => content::put_manifests(state, manifests).await,
        Request::GetManifests { ids } => {
            check_batch(ids.len())?;
            edge::fill_manifests(state, &ids).await?;
            content::get_manifests(state, &access, ids)
        }
        Request::GetContentChunks { contents, ids } => {
            check_batch(contents.len())?;
            check_batch(ids.len())?;
            edge::fill_manifests(state, &contents).await?;
            content::check_content_chunks(state, &access, &contents, &ids)?;
            edge::fill_chunks(state, &ids).await?;
            content::get_chunks(state, ids, send).await
        }
        _ => unreachable!("dispatched by area"),
    }
}

async fn changes(state: &Arc<State>, access: Access, request: Request) -> Response {
    match request {
        Request::Submit {
            stream,
            workspace,
            description,
            changes,
            integration,
            review,
            fixes,
            moves,
        } => {
            let args = change::SubmitArgs {
                stream,
                workspace,
                description,
                changes,
                integration,
                review,
                fixes,
                moves,
            };
            change::submit(state, access, args).await
        }
        Request::Filelog { path } => change::filelog(state, &access, path),
        Request::Changes { stream, limit } => change::changes(state, &access, stream, limit),
        Request::Describe { cl } => change::describe(state, &access, cl),
        Request::AcquireLocks {
            stream,
            workspace,
            paths,
        } => change::acquire_locks(state, &access, stream, workspace, paths).await,
        Request::ReleaseLocks {
            stream,
            workspace,
            paths,
            force,
        } => change::release_locks(state, &access, stream, workspace, paths, force).await,
        Request::ListLocks { stream } => change::list_locks(state, &access, stream),
        Request::Shelve {
            shelf,
            stream,
            workspace,
            description,
            files,
        } => change::shelve(state, &access, shelf, stream, workspace, description, files).await,
        Request::GetShelf { cl } => change::get_shelf(state, &access, cl),
        Request::DeleteShelf { cl } => change::delete_shelf(state, &access, cl).await,
        _ => unreachable!("dispatched by area"),
    }
}

async fn admin(state: &Arc<State>, access: Access, request: Request) -> Response {
    match request {
        Request::ClusterStatus => {
            access.require_role(kelp_meta::acl::Role::Operations)?;
            let cluster = state.cluster().ok_or_else(|| {
                ErrorReply::new(ErrorKind::Invalid, "this server is not in a commit group")
            })?;
            Ok(Reply::Cluster(cluster.status()))
        }
        Request::Gc { dry_run } => admin::gc(state, &access, dry_run).await,
        Request::Obliterate { paths } => admin::obliterate(state, access, paths).await,
        Request::AuditLog { from, limit } => admin::audit_log(state, &access, from, limit),
        Request::VerifyAudit => admin::verify_audit(state, &access).await,
        Request::Tier => admin::tier(state, &access).await,
        Request::Backup {
            dest,
            metadata_only,
        } => admin::backup(state, &access, dest, metadata_only).await,
        Request::Fsck { deep } => admin::fsck(state, &access, deep).await,
        Request::ReadLog {
            from,
            limit,
            wait_ms,
        } => admin::read_log(state, &access, from, limit, wait_ms).await,
        _ => unreachable!("dispatched by area"),
    }
}

async fn artifacts(state: &Arc<State>, access: Access, request: Request) -> Response {
    match request {
        Request::PublishArtifact {
            stream,
            cl,
            name,
            tags,
            files,
        } => artifact::publish(state, access, stream, cl, name, tags, files).await,
        Request::FindArtifacts {
            stream,
            name,
            at,
            tags,
            limit,
        } => artifact::find(state, &access, stream, name, at, tags, limit),
        Request::ArtifactFiles { id } => artifact::files(state, access, id).await,
        Request::GetArtifact { id } => artifact::get(state, &access, id),
        Request::PinArtifact { id, pinned } => {
            artifact::admin(
                state,
                &access,
                kelp_meta::Command::PinArtifact { id, pinned },
            )
            .await
        }
        Request::DeleteArtifact { id } => {
            artifact::admin(state, &access, kelp_meta::Command::DeleteArtifact { id }).await
        }
        Request::SetArtifactRetention {
            stream,
            name,
            keep_last,
        } => artifact::retention(state, &access, stream, name, keep_last).await,
        _ => unreachable!("dispatched by area"),
    }
}

async fn governance(state: &Arc<State>, access: Access, request: Request) -> Response {
    match request {
        Request::OpenReview { shelf, reviewers } => {
            review::open(state, &access, shelf, reviewers).await
        }
        Request::Vote {
            shelf,
            version,
            approve,
        } => review::vote(state, &access, shelf, version, approve).await,
        Request::ReportStatus {
            shelf,
            version,
            context,
            state: check,
            url,
        } => review::report_status(state, &access, shelf, version, context, check, url).await,
        Request::CloseReview { shelf } => review::close(state, &access, shelf).await,
        Request::GetReview { shelf } => review::get(state, access, shelf).await,
        Request::ListReviews { stream, open_only } => {
            review::list(state, access, stream, open_only).await
        }
        Request::GetReviewRules => review::get_rules(state),
        Request::SetReviewRules { source } => review::set_rules(state, &access, source).await,
        Request::SetWebhook {
            name,
            url,
            secret,
            events,
            paths,
        } => webhook::set(state, &access, name, url, secret, events, paths).await,
        Request::DeleteWebhook { name } => webhook::delete(state, &access, name).await,
        Request::ListWebhooks => webhook::list(state, &access),
        Request::Deliveries { webhook, limit } => {
            webhook::deliveries(state, &access, webhook, limit)
        }
        Request::Redeliver { id } => webhook::redeliver(state, &access, id).await,
        Request::RegisterPlugin {
            name,
            version,
            content,
            size,
        } => plugin::register(state, &access, name, version, content, size).await,
        Request::DeletePlugin { name, version } => {
            plugin::delete(state, &access, name, version).await
        }
        Request::GetPlugin { name, version } => plugin::get(state, name, version),
        Request::ListPlugins => plugin::list(state),
        Request::GetIssue { key } => issue::get(state, access, key).await,
        Request::LinkIssues { cl, add, remove } => {
            issue::link(state, &access, cl, add, remove).await
        }
        Request::IssuesIn { stream, at, since } => {
            issue::issues_in(state, access, stream, at, since).await
        }

        _ => unreachable!("dispatched by area"),
    }
}

async fn labels(state: &Arc<State>, access: Access, request: Request) -> Response {
    match request {
        Request::SetLabel {
            name,
            stream,
            cl,
            description,
        } => label::set(state, &access, name, stream, cl, description).await,
        Request::DeleteLabel { name } => label::delete(state, &access, name).await,
        Request::GetLabel { name } => label::get(state, &access, name),
        Request::ListLabels => label::list(state, &access),
        _ => unreachable!("dispatched by area"),
    }
}
