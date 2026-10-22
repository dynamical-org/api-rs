//! Documentation HTML adapted from FastAPI's MIT-licensed templates.
//! See LICENSE-fastapi and LICENSE-swagger-ui for the OAuth redirect source.
use crate::{HttpResponse, openapi, response};

pub(crate) fn serve(path: &str) -> HttpResponse {
    let (bytes, media) = match path {
        "/openapi.json" => (openapi::bytes(), "application/json"),
        "/docs" => (
            include_bytes!("../docs/swagger.html").as_slice(),
            "text/html; charset=utf-8",
        ),
        "/docs/oauth2-redirect" => (
            include_bytes!("../docs/oauth2-redirect.html").as_slice(),
            "text/html; charset=utf-8",
        ),
        "/redoc" => (
            include_bytes!("../docs/redoc.html").as_slice(),
            "text/html; charset=utf-8",
        ),
        _ => unreachable!(),
    };
    response(200, bytes.to_vec(), Some(media))
}
