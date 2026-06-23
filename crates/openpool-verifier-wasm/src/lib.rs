//! Browser binding for the exact native OPENPOOL-1 verifier.
use wasm_bindgen::prelude::*;

/// Validates a canonical proof JSON document and returns the native verifier report as JSON.
/// No network request, provider API, or browser-side algorithm recreation is involved.
#[wasm_bindgen]
pub fn verify_proof_json(document: &str) -> Result<String, JsValue> {
    let proof: openpool_protocol::ProofDocument = serde_json::from_str(document)
        .map_err(|error| JsValue::from_str(&format!("invalid proof JSON: {error}")))?;
    serde_json::to_string(&openpool_verifier::verify(&proof)).map_err(|error| {
        JsValue::from_str(&format!("could not encode verification report: {error}"))
    })
}
