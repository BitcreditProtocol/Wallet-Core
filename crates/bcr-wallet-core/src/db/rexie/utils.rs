// ----- standard library im
// ----- extra library imports
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_wasm_bindgen::{from_value, to_value};
use wasm_bindgen::JsValue;
// ----- local modules
use crate::db::types::DatabaseError;
// ----- end imports

pub fn to_js<T: Serialize>(value: &T) -> Result<JsValue, DatabaseError> {
    to_value(value)
        .map_err(|e| DatabaseError::SerializationError(format!("Cannot convert into JS: {:?}", e)))
}

pub fn from_js<T: DeserializeOwned>(js: JsValue) -> Result<T, DatabaseError> {
    from_value(js)
        .map_err(|e| DatabaseError::SerializationError(format!("Cannot convert from JS: {:?}", e)))
}
