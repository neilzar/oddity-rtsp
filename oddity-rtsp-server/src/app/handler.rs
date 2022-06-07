use std::sync::Arc;

use tokio::sync::{Mutex, MutexGuard};

use oddity_rtsp_protocol::{
  Request,
  Response,
  Method,
  Status,
  Transport,
};

use crate::net::connection::ResponseSenderTx;
use crate::session::SessionId;
use crate::session::session_manager::RegisterSessionError;
use crate::session::setup::{SessionSetup, SessionSetupError};
use crate::app::AppContext;

pub struct AppHandler {
  context: Arc<Mutex<AppContext>>,
}

impl AppHandler {

  pub fn new(context: Arc<Mutex<AppContext>>) -> Self {
    Self {
      context,
    }
  }

  pub async fn handle(
    &self,
    request: &Request,
    responder: ResponseSenderTx,
  ) -> Response {
    tracing::trace!(%request, "handling request");

    // Check the Require header and make sure all requested options are
    // supported or return response with 551 Option Not Supported.
    if !is_request_require_supported(request) {
      return reply_option_not_supported(request);
    }

    match request.method {
      /* Stateless */
      Method::Options => {
        tracing::trace!("handling OPTIONS request");
        reply_to_options_with_supported_methods(request)
      },
      Method::Announce => {
        tracing::trace!("handling ANNOUNCE request");
        reply_method_not_supported(request)
      },
      Method::Describe => {
        tracing::trace!("handling DESCRIBE request");
        if is_request_one_of_content_types_supported(request) {
          tracing::trace!(path=request.path(), "querying SDP file for source");
          match self
              .use_context()
              .await
              .source_manager
              .describe(request.path())
              .await {
            Some(Ok(sdp_contents)) => {
              tracing::trace!(path=request.path(), %sdp_contents, "have SDP");
              reply_to_describe_with_media_sdp(
                request,
                sdp_contents.to_string(),
              )
            },
            Some(Err(err)) => {
              tracing::error!(%request, %err, "failed to query SDP of media source");
              reply_internal_server_error(request)
            },
            None => {
              reply_not_found(request)
            },
          }
        } else {
          tracing::warn!(
            %request,
            "none of content types accepted by client are supported, \
             server only supports `application/sdp`",
          );
          reply_not_acceptable(request)
        }
      },
      Method::GetParameter => {
        tracing::trace!("handling GET_PARAMETER request");
        reply_method_not_supported(request)
      },
      Method::SetParameter => {
        tracing::trace!("handling SET_PARAMETER request");
        reply_method_not_supported(request)
      },
      /* Stateful */
      Method::Setup => {
        tracing::trace!("handling SETUP request");
        if request.session().is_some() {
          // RFC specification allows negatively responding to SETUP request with Session
          // IDs by responding with 459 Aggregate Operation Not Allowed. By handling this
          // here we don't have to deal with clients trying to change transport parameters
          // on media items that are already playing.
          return reply_aggregate_operation_not_allowed(request);
        }

        let transport = match request.transport() {
          Ok(transport) => transport,
          Err(_) => {
            // If the client did not provide a valid transport header value, then there
            // no way to reach it and we return "Unsupported Transport".
            return reply_unsupported_transport(request);
          }
        };
        tracing::trace!(path=request.path(), ?transport, "resolved transport");

        let mut source_delegate = match self
            .use_context()
            .await
            .source_manager
            .subscribe(request.path())
            .await {
          Some(source_delegate) => source_delegate,
          None => {
            return reply_not_found(request);
          },
        };
        tracing::trace!(path=request.path(), "acquired source delegate");

        let media_info = match source_delegate.query_media_info().await {
          Some(media_info) => media_info,
          None => {
            tracing::trace!(
              path=request.path(),
              "failed to query media info from source",
            );
            return reply_internal_server_error(request);
          },
        };

        let session_setup = match SessionSetup::from_rtsp_candidate_transports(
          transport,
          media_info,
          responder,
        ).await {
          Ok(session_setup) => session_setup,
          Err(SessionSetupError::TransportNotSupported) |
          Err(SessionSetupError::DestinationInvalid) => {
            return reply_unsupported_transport(request);
          },
          Err(SessionSetupError::Media(err)) => {
            tracing::error!(
              %request, %err,
              "failed to setup session for media source",
            );
            return reply_internal_server_error(request)
          },
        };
        tracing::trace!(path=request.path(), "setup session");

        let transport = session_setup.rtsp_transport.clone();
        match self
            .use_context()
            .await
            .session_manager
            .setup_and_start(source_delegate, session_setup)
            .await {
          // Session was successfully registered!
          Ok(session_id) => {
            tracing::trace!(path=request.path(), %session_id, "registered session");
            reply_to_setup(request, &session_id, &transport)
          },
          // In the highly unlikely case that the randomly generated session was already
          // in use before.
          Err(RegisterSessionError::AlreadyRegistered) => {
            tracing::error!(
              %request,
              "session id already present (collision)");
            reply_internal_server_error(request)
          },
        }
      },
      Method::Play => {
        tracing::trace!("handling PLAY request");
        // TODO!
        unimplemented!();
      },
      Method::Pause => {
        tracing::trace!("handling PAUSE request");
        reply_method_not_supported(request)
      },
      Method::Record => {
        tracing::trace!("handling RECORD request");
        reply_method_not_supported(request)
      },
      Method::Teardown => {
        tracing::trace!("handling TEARDOWN request");
        // TODO!
        unimplemented!();
      },
      /* Invalid */
      // Request with method REDIRECT can only be sent from server to
      // client, not the other way around.
      Method::Redirect => {
        tracing::trace!("handling REDIRECT request");
        reply_method_not_valid(request)
      },
    }
  }

  #[inline]
  async fn use_context(&self) -> MutexGuard<'_, AppContext> {
    self.context.lock().await
  }

}

#[inline]
fn is_request_require_supported(
  request: &Request
) -> bool {
  // We don't support any features at this point
  request.require().is_none()
}

#[inline]
fn is_request_one_of_content_types_supported(
  request: &Request,
) -> bool {
  // We only support SDP
  request.accept().contains(&"application/sdp")
}

#[inline]
fn reply_to_options_with_supported_methods(
  request: &Request,
) -> Response {
  Response::ok()
    .with_cseq_of(request)
    .with_header(
      "Public",
      "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN")
    .build()
}

#[inline]
fn reply_to_describe_with_media_sdp(
  request: &Request,
  sdp_contents: String,
) -> Response {
  Response::ok()
    .with_cseq_of(request)
    .with_sdp(sdp_contents)
    .build()
}

#[inline]
fn reply_to_setup(
  request: &Request,
  session_id: &SessionId,
  transport: &Transport,
) -> Response {
  Response::ok()
    .with_cseq_of(request)
    .with_header("Session", session_id)
    .with_header("Transport", transport)
    .build()
}

#[inline]
fn reply_option_not_supported(
  request: &Request,
) -> Response {
  tracing::debug!(
    %request,
    "client asked for feature that is not supported");
  Response::error(Status::OptionNotSupported)
    .with_cseq_of(request)
    .build()
}

#[inline]
fn reply_method_not_supported(
  request: &Request,
) -> Response {
  tracing::warn!(
    %request,
    method = %request.method,
    "client sent unsupported request");
  Response::error(Status::MethodNotAllowed)
    .with_cseq_of(request)
    .build()
}

#[inline]
fn reply_method_not_valid(
  request: &Request,
) -> Response {
  tracing::warn!(
    %request,
    method = %request.method,
    "client tried server-only method in request to server; \
    does client think it is server?");
  Response::error(Status::MethodNotValidInThisState)
    .with_cseq_of(request)
    .build()
}

#[inline]
fn reply_not_acceptable(
  request: &Request,
) -> Response {
  tracing::debug!(
    %request,
    "server does not support a presentation format acceptable \
    by client");
  Response::error(Status::NotAcceptable)
    .with_cseq_of(request)
    .build()
}

#[inline]
fn reply_not_found(
  request: &Request,
) -> Response {
  tracing::debug!(
    %request,
    path = request.path(),
    "path not registered as media item");
  Response::error(Status::NotFound)
    .with_cseq_of(request)
    .build()
}

#[inline]
fn reply_aggregate_operation_not_allowed(
  request: &Request,
) -> Response {
  tracing::debug!(
    %request,
    "refusing to do aggregate request");
  Response::error(Status::AggregateOperationNotAllowed)
    .with_cseq_of(request)
    .build()
}

#[inline]
fn reply_unsupported_transport(
  request: &Request,
) -> Response {
  tracing::debug!(
    %request,
    "unsupported transport");
  Response::error(Status::UnsupportedTransport)
    .with_cseq_of(request)
    .build()
}

#[inline]
fn reply_internal_server_error(
  request: &Request,
) -> Response {
  Response::error(Status::InternalServerError)
    .with_cseq_of(request)
    .build()
}