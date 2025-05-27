use wasm_bindgen::prelude::*;

#[derive(Debug)]
pub enum Error {
    SomeError(String),
}

impl From<Error> for JsValue {
    fn from(error: Error) -> JsValue {
        "Nooo".into()
    }
}
