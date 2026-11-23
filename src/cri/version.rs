use crate::proto::runtime::v1::{VersionRequest, VersionResponse};
use tonic::{Request, Response, Status};

const KUBE_API_VERSION: &str = "0.1.0";
const CONTAINER_NAME: &str = "nautilusd";

pub async fn version(
    _request: Request<VersionRequest>,
) -> Result<Response<VersionResponse>, Status> {
    let response = VersionResponse {
        version: KUBE_API_VERSION.to_string(),
        runtime_name: CONTAINER_NAME.to_string(),
        runtime_version: "0.0.1".to_string(),
        runtime_api_version: "v1".to_string(),
    };
    Ok(Response::new(response))
}
