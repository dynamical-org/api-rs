use base64::{Engine, engine::general_purpose::STANDARD};
use dynapi_reader::{
    HttpVirtualChunkContainer, Selector, StoreDescriptor, read_cf_selection_traced_with_containers,
    read_selection_traced_with_containers,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Read};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    store_descriptor: StoreDescriptor,
    snapshot_id: String,
    array_path: String,
    selection: Vec<Selector>,
    cf_time: Option<dynapi_reader::cf_time::Request>,
    #[serde(default)]
    virtual_chunk_containers: Vec<HttpVirtualChunkContainer>,
}

async fn run() -> Value {
    let mut input = Vec::new();
    if let Err(e) = io::stdin()
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut input)
    {
        return json!({"error":{"kind":"InvalidRequest","message":e.to_string()}});
    }
    if input.len() > 8 * 1024 * 1024 {
        return json!({"error":{"kind":"SizeLimit","message":"Request exceeds 8 MiB"}});
    }
    let request: Request = match serde_json::from_slice(&input) {
        Ok(request) => request,
        Err(e) => return json!({"error":{"kind":"InvalidRequest","message":e.to_string()}}),
    };
    let result = if let Some(cf_request) = &request.cf_time {
        read_cf_selection_traced_with_containers(
            &request.store_descriptor,
            &request.snapshot_id,
            &request.array_path,
            &request.selection,
            cf_request,
            &request.virtual_chunk_containers,
        )
        .await
    } else {
        read_selection_traced_with_containers(
            &request.store_descriptor,
            &request.snapshot_id,
            &request.array_path,
            &request.selection,
            &request.virtual_chunk_containers,
        )
        .await
    };
    match result {
        Ok((buffer, trace)) => {
            if std::env::var("DYNAPI_READ_TRACE").as_deref() == Ok("1") {
                eprintln!("{}", json!(trace));
            }
            json!({"dtype":buffer.dtype,"shape":buffer.shape,"data_b64":STANDARD.encode(buffer.bytes)})
        }
        Err(error) => json!({"error":error}),
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    println!("{}", run().await);
}
