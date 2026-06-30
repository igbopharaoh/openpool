//! Dioxus SSR/hydration UI for the OpenPool public and authenticated surfaces.

use dioxus::prelude::*;
use openpool_core::contract::{OperatorJob, PublicInvoice, PublicPool, PublicRaffle, PublicResult};
use serde::de::DeserializeOwned;

#[derive(Clone, Debug, PartialEq, Routable)]
pub enum Route {
    #[route("/")]
    Home {},
    #[route("/raffles")]
    Raffles {},
    #[route("/raffles/:id")]
    Raffle { id: String },
    #[route("/organizer")]
    Organizer {},
    #[route("/operator")]
    Operator {},
    #[route("/verify")]
    Verify {},
    #[route("/:..segments")]
    NotFound { segments: Vec<String> },
}

#[component]
pub fn App() -> Element {
    rsx! { Router::<Route> {} }
}

#[component]
fn Shell(children: Element) -> Element {
    rsx! {
        main { class: "openpool-shell",
            nav { class: "navigation", aria_label: "OpenPool navigation",
                Link { to: Route::Home {}, "OpenPool" }
                Link { to: Route::Raffles {}, "Raffles" }
                Link { to: Route::Organizer {}, "Organizer" }
                Link { to: Route::Verify {}, "Verify a proof" }
            }
            {children}
        }
    }
}

#[component]
fn Home() -> Element {
    rsx! { Shell {
        p { class: "eyebrow", "OPENPOOL" }
        h1 { "Verifiable Lightning raffles." }
        p { "OpenPool publishes the facts needed to reproduce a raffle outcome without exposing participant payment data." }
        Link { class: "button", to: Route::Raffles {}, "Browse raffles" }
    } }
}

#[component]
fn Raffles() -> Element {
    let raffles = use_resource(|| async { api_get::<Vec<PublicRaffle>>("/v1/raffles").await });
    rsx! { Shell {
        p { class: "eyebrow", "PUBLIC RAFFLES" }
        h1 { "Browse raffles" }
        p { "Live catalogue data comes from the OpenPool API." }
        match raffles() {
            Some(Ok(items)) if items.is_empty() => rsx! { section { class: "empty-state", "No raffles are currently published." } },
            Some(Ok(items)) => rsx! { for raffle in items { Link { to: Route::Raffle { id: raffle.id.to_string() }, RaffleCard { raffle } } } },
            Some(Err(error)) => rsx! { section { class: "empty-state", "Catalogue unavailable: {error}" } },
            None => rsx! { section { class: "empty-state", "Loading catalogue…" } },
        }
    } }
}

#[component]
fn Raffle(id: String) -> Element {
    let raffle_id = id.clone();
    let detail = use_resource(move || {
        let id = raffle_id.clone();
        async move { raffle_detail(&id).await }
    });
    rsx! { Shell {
        p { class: "eyebrow", "PUBLIC RAFFLE" }
        h1 { "Raffle details" }
        match detail() {
            Some(Ok(detail)) => rsx! {
                section { class: "card", h2 { "{detail.raffle.name}" }
                    p { "{detail.raffle.entry_price_sats} sats per ticket · {detail.raffle.status}" }
                    p { "Confirmed pool: {detail.pool.total_pool_sats} sats / {detail.pool.total_tickets} tickets" }
                }
                InvoicePanel { raffle_id: id.clone() }
                ResultCard { result: detail.result }
                Link { class: "button", to: Route::Verify {}, "Verify terminal proof locally" }
            },
            Some(Err(error)) => rsx! { section { class: "empty-state", "Raffle unavailable: {error}" } },
            None => rsx! { section { class: "empty-state", "Loading raffle…" } },
        }
    } }
}

#[derive(Clone)]
struct RaffleDetail {
    raffle: PublicRaffle,
    pool: PublicPool,
    result: PublicResult,
}

async fn raffle_detail(id: &str) -> Result<RaffleDetail, String> {
    Ok(RaffleDetail {
        raffle: api_get(&format!("/v1/raffles/{id}")).await?,
        pool: api_get(&format!("/v1/raffles/{id}/pool")).await?,
        result: api_get(&format!("/v1/raffles/{id}/result")).await?,
    })
}

#[component]
fn InvoicePanel(raffle_id: String) -> Element {
    let mut address = use_signal(String::new);
    let mut tickets = use_signal(|| "1".to_owned());
    let mut result = use_signal(|| None::<Result<PublicInvoice, String>>);
    let submit_id = raffle_id.clone();
    rsx! { section { class: "card",
        h2 { "Buy entries" }
        label { "Lightning Address", input { value: "{address}", oninput: move |event| address.set(event.value()) } }
        label { "Tickets", input { r#type: "number", min: "1", value: "{tickets}", oninput: move |event| tickets.set(event.value()) } }
        button { class: "button", onclick: move |_| {
            let address = address(); let ticket_count = tickets().parse::<u64>().unwrap_or(0); let id = submit_id.clone();
            spawn(async move { result.set(Some(api_post(&format!("/v1/raffles/{id}/invoices"), &serde_json::json!({"lightning_address": address, "ticket_count": ticket_count})).await)); });
        }, "Create Lightning invoice" }
        match result() {
            Some(Ok(invoice)) => rsx! { p { "Invoice {invoice.id}: {invoice.status}; {invoice.amount_sats} sats" } if let Some(bolt11) = invoice.bolt11 { code { "{bolt11}" } } },
            Some(Err(error)) => rsx! { p { "Invoice request failed: {error}" } },
            None => rsx! { p { "Settlement and ticket range will update from the invoice-status API." } },
        }
    } }
}

#[component]
pub fn ResultCard(result: PublicResult) -> Element {
    let winner = result
        .winning_ticket
        .map(|ticket| format!("Winning ticket: {ticket}"))
        .unwrap_or_else(|| "Winner has not been selected yet.".into());
    let proof = if result.proof_available {
        "Terminal proof is available for local verification."
    } else {
        "Terminal proof is not available yet."
    };
    let payouts = result.payout_statuses.join(", ");
    rsx! {
        section { class: "card raffle-result", aria_live: "polite",
            h2 { "Raffle result" }
            p { "Status: {result.status}" }
            p { "{winner}" }
            p { "Payout states: {payouts}" }
            p { "{proof}" }
        }
    }
}

#[component]
fn Organizer() -> Element {
    let raffles =
        use_resource(|| async { api_get::<Vec<PublicRaffle>>("/v1/organizers/me/raffles").await });
    rsx! { Shell {
        p { class: "eyebrow", "ORGANIZER" }
        h1 { "Organizer dashboard" }
        p { "OIDC-authenticated organizers create, schedule, open, and monitor their raffles here." }
        match raffles() {
            Some(Ok(items)) => rsx! { for raffle in items { RaffleCard { raffle } } },
            Some(Err(error)) => rsx! { a { class: "button", href: "/auth/login", "Sign in to manage raffles" } p { "Organizer data unavailable: {error}" } },
            None => rsx! { p { "Loading organizer lifecycle…" } },
        }
    } }
}

#[component]
fn Operator() -> Element {
    let jobs = use_resource(|| async { api_get::<Vec<OperatorJob>>("/v1/operator/jobs").await });
    rsx! { Shell {
        p { class: "eyebrow", "OPERATOR" }
        h1 { "Operator console" }
        p { "Authorized operators inspect jobs and payment state, then retry permitted side effects without mutating historical facts." }
        match jobs() {
            Some(Ok(items)) if items.is_empty() => rsx! { p { "No recent jobs." } },
            Some(Ok(items)) => rsx! { for job in items { section { class: "card", h2 { "{job.kind}" } p { "{job.status}; attempt {job.attempts}/{job.max_attempts}" } if let Some(error) = job.last_error { p { "{error}" } } } } },
            Some(Err(error)) => rsx! { a { class: "button", href: "/auth/login", "Sign in as operator" } p { "Operator data unavailable: {error}" } },
            None => rsx! { p { "Loading operational queue…" } },
        }
    } }
}

#[component]
fn Verify() -> Element {
    let mut source = use_signal(String::new);
    let mut verification = use_signal(|| None::<String>);
    rsx! { Shell {
        p { class: "eyebrow", "INDEPENDENT VERIFICATION" }
        h1 { "Verify an OpenPool proof" }
        p { "The browser runs the same Rust OPENPOOL-1 verifier compiled to WebAssembly; no private API call is used for validation." }
        textarea { value: "{source}", placeholder: "Paste proof.json", oninput: move |event| source.set(event.value()) }
        button { class: "button", onclick: move |_| { verification.set(Some(match openpool_verifier::verify_proof_json(&source()) { Ok(report) => report, Err(error) => format!("Invalid proof: {error:?}"), })); }, "Verify locally" }
        if let Some(report) = verification() { pre { "{report}" } }
    } }
}

#[component]
fn NotFound(segments: Vec<String>) -> Element {
    let path = segments.join("/");
    rsx! { Shell {
        h1 { "Page not found" }
        p { "No OpenPool page exists at /{path}." }
        Link { to: Route::Home {}, "Return home" }
    } }
}

#[component]
pub fn RaffleCard(raffle: PublicRaffle) -> Element {
    rsx! {
        article { class: "raffle-card",
            span { class: "status", "{raffle.status}" }
            h2 { "{raffle.name}" }
            p { "{raffle.entry_price_sats} sats per ticket" }
            p { "{raffle.total_pool_sats} sats confirmed" }
        }
    }
}

/// Server-rendered shell mounted as the Axum fallback. The browser hydration target is added
/// alongside the verifier WASM package before the technical-staging release.
pub fn render_home() -> String {
    dioxus::ssr::render_element(rsx! { App {} })
}

async fn api_get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    reqwest::get(api_url(path))
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

async fn api_post<T: DeserializeOwned>(path: &str, body: &serde_json::Value) -> Result<T, String> {
    reqwest::Client::new()
        .post(api_url(path))
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

fn api_url(path: &str) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        format!(
            "{}{}",
            web_sys::window()
                .and_then(|w| w.location().origin().ok())
                .unwrap_or_default(),
            path
        )
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        format!(
            "{}{}",
            std::env::var("OPENPOOL_PUBLIC_API_BASE")
                .unwrap_or_else(|_| "http://127.0.0.1:3000".into())
                .trim_end_matches('/'),
            path
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssr_shell_contains_public_navigation() {
        let html = render_home();
        assert!(html.contains("OPENPOOL"));
        assert!(html.contains("Browse raffles"));
        assert!(html.contains("Verify a proof"));
    }

    #[test]
    fn result_card_renders_terminal_proof_state() {
        let html = dioxus::ssr::render_element(rsx! { ResultCard {
            result: PublicResult { raffle_id: uuid::Uuid::nil(), status: "PAID_OUT".into(), winning_ticket: Some(4), payout_statuses: vec!["settled".into()], proof_available: true }
        } });
        assert!(html.contains("Winning ticket: 4"));
        assert!(html.contains("Terminal proof is available"));
    }
}
