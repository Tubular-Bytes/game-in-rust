use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
pub struct BuildParams {
    pub blueprint: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ApiRequest<T = BuildParams> {
    pub id: String,
    pub method: String,
    pub params: T,
}
