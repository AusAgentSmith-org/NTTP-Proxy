use crate::store::Store;

pub struct Config {
    pub admin_token: String,
    pub proxy_token: String,
}

pub struct AppState {
    pub store: Store,
    pub config: Config,
}
