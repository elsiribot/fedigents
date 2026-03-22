use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use gloo_timers::future::sleep;
use js_sys::Date;
use leptos::ev::{KeyboardEvent, MouseEvent, SubmitEvent};
use leptos::html::Textarea;
use leptos::prelude::*;
use leptos_qr_scanner::Scan;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlTextAreaElement;

use crate::agent::{
    assistant_message, load_session, load_sessions_index, load_skills, onboarding_message,
    save_session, user_message, ChatMessage, ChatRole, ConversationLog, PaymentKind,
    PendingPaymentProposal,
    SkillSummary, StoredSession, WalletAgent,
};
use crate::browser;
use crate::ppq::{PpqAccount, PpqClient, PpqModel};
use crate::wallet_runtime::{BootstrapEvent, OperationEvent, WalletRuntime};
use gloo_storage::{LocalStorage, Storage};

const PPQ_TOPUP_USD: f64 = 0.10;
const PPQ_LOW_BALANCE_USD: f64 = 0.05;
const PPQ_BALANCE_POLL_INTERVAL: Duration = Duration::from_secs(60);
const PPQ_TOPUP_GRACE_MS: f64 = 120_000.0;
const DEFAULT_MODEL: &str = "openai/gpt-5.4-nano";
const MODEL_STORAGE_KEY: &str = "fedigents.model";
const THINKING_EFFORT_KEY: &str = "fedigents.thinking_effort";

#[component]
pub fn App() -> impl IntoView {
    let session_id = RwSignal::new(format!(
        "fedigents.session.{}",
        Date::now() as u64
    ));
    let session_created_at = RwSignal::new(Date::now());
    let conversation = RwSignal::new(ConversationLog::new());
    let sessions_index = RwSignal::new(load_sessions_index());
    let messages = RwSignal::new(vec![onboarding_message(
        "Booting Fedigents. Opening local wallet storage.",
    )]);
    let prompt = RwSignal::new(String::new());
    let status = RwSignal::new("Preparing wallet".to_owned());
    let balance = RwSignal::new("...".to_owned());
    let receive_code = RwSignal::new(None::<String>);
    let skills = RwSignal::new(Vec::<SkillSummary>::new());
    let runtime = Rc::new(RefCell::new(None::<WalletRuntime>));
    let agent = Rc::new(RefCell::new(None::<WalletAgent>));
    let ready = RwSignal::new(false);
    let busy = RwSignal::new(true);
    let confirming_payment = RwSignal::new(false);
    let pending_payment = RwSignal::new(None::<PendingPaymentProposal>);
    let payment_result = RwSignal::new(None::<Result<String, String>>);
    let scanner_open = RwSignal::new(false);
    let debug_mode = RwSignal::new(false);
    let paid_invoices: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());
    let payment_received_trigger: RwSignal<Option<String>> = RwSignal::new(None);
    let menu_open = RwSignal::new(false);
    let ppq_account = RwSignal::new(None::<PpqAccount>);
    let ppq_low_balance_usd = RwSignal::new(None::<f64>);
    let ppq_balance_usd = RwSignal::new(None::<f64>);
    let ppq_topup_in_progress = RwSignal::new(false);
    let ppq_topup_grace_until = Rc::new(RefCell::new(None::<f64>));
    let selected_model = RwSignal::new(
        LocalStorage::get::<String>(MODEL_STORAGE_KEY)
            .unwrap_or_else(|_| DEFAULT_MODEL.to_owned()),
    );
    let available_models = RwSignal::new(Vec::<PpqModel>::new());
    let thinking_effort = RwSignal::new(
        LocalStorage::get::<String>(THINKING_EFFORT_KEY).unwrap_or_default(),
    );
    let low_balance_threshold = RwSignal::new(PPQ_LOW_BALANCE_USD);
    let textarea_ref = NodeRef::<Textarea>::new();

    let effect_runtime = Rc::clone(&runtime);
    let effect_agent = Rc::clone(&agent);
    let effect_ppq_topup_grace_until = Rc::clone(&ppq_topup_grace_until);
    Effect::new(move |_| {
        spawn_local(async move {
            match PpqClient::new().list_models().await {
                Ok(models) => available_models.set(models),
                Err(err) => tracing::warn!("Failed to load PPQ model catalog: {err}"),
            }
        });

        let runtime_cell = Rc::clone(&effect_runtime);
        let agent_cell = Rc::clone(&effect_agent);
        let ppq_topup_grace_until = Rc::clone(&effect_ppq_topup_grace_until);
        spawn_local(async move {
            if let Err(err) = browser::ensure_service_worker().await {
                push_message(
                    &messages,
                    onboarding_message(format!("Service worker registration failed: {err}")),
                );
            }

            match load_skills().await {
                Ok(loaded) => skills.set(loaded),
                Err(err) => push_message(
                    &messages,
                    onboarding_message(format!("Skill catalog failed to load: {err}")),
                ),
            }

            let wallet = match WalletRuntime::connect().await {
                Ok(runtime_value) => runtime_value,
                Err(err) => {
                    status.set("Wallet failed to boot".to_owned());
                    busy.set(false);
                    push_message(
                        &messages,
                        onboarding_message(format!("Wallet setup failed: {err}")),
                    );
                    return;
                }
            };

            if let Some(notice) = wallet.storage_notice() {
                push_message(&messages, onboarding_message(notice));
            }

            runtime_cell.borrow_mut().replace(wallet.clone());

            let bootstrap_result = wallet
                .bootstrap(move |event| match event {
                    BootstrapEvent::Note(note) => {
                        status.set(note.clone());
                        push_message(&messages, onboarding_message(note));
                    }
                    BootstrapEvent::ReceiveCode(code) => {
                        receive_code.set(Some(code.clone()));
                        push_message(
                            &messages,
                            onboarding_message(
                                "Your wallet receive LNURL is ready. Use it to fund the wallet.",
                            ),
                        );
                    }
                    BootstrapEvent::Balance(amount_sats) => {
                        balance.set(format_balance(amount_sats));
                    }
                })
                .await;

            if let Err(err) = bootstrap_result {
                status.set("Wallet bootstrap failed".to_owned());
                busy.set(false);
                push_message(
                    &messages,
                    onboarding_message(format!("Bootstrap failed: {err}")),
                );
                return;
            }

            // Start watching for incoming payments in the background.
            {
                let listener_wallet = wallet.clone();
                let watcher_wallet = wallet.clone();
                wallet.set_operation_listener(Some(Rc::new(move |event| match event {
                    OperationEvent::PaymentReceived { amount_sats, invoice } => {
                        if let Some(inv) = &invoice {
                            paid_invoices.update(|set| { set.insert(inv.clone()); });
                        }
                        let msg = match amount_sats {
                            Some(sats) => format!("Incoming payment of {sats} sats received."),
                            None => "Incoming payment received.".to_owned(),
                        };
                        push_message(&messages, onboarding_message(msg.clone()));
                        payment_received_trigger.set(Some(msg));
                        let wallet = listener_wallet.clone();
                        spawn_local(async move {
                            if let Ok(sats) = wallet.get_balance().await {
                                balance.set(format_balance(sats));
                            }
                        });
                    }
                })));
                spawn_local(async move {
                    if let Err(err) = watcher_wallet.watch_pending_receives().await {
                        tracing::warn!("Failed to start background receive watchers: {err}");
                    }
                });
            }

            match wallet.is_ppq_ready().await {
                Ok(false) => {
                    if let Ok(true) = wallet.ppq_funding_in_flight().await {
                        busy.set(false);
                        status.set("PPQ setup needs recovery".to_owned());
                        push_message(
                            &messages,
                            onboarding_message(
                                "PPQ funding previously started but final setup state was not saved. Chat stays locked to avoid double-funding.",
                            ),
                        );
                        return;
                    }

                    let ppq = PpqClient::new();
                    push_message(
                        &messages,
                        onboarding_message("Creating a PPQ account and funding it with $0.10..."),
                    );
                    match fund_ppq(&wallet, &ppq).await {
                        Ok(account) => match wallet.mark_ppq_ready().await {
                            Ok(()) => {
                                agent_cell.borrow_mut().replace(WalletAgent::new(
                                    wallet.clone(),
                                    ppq,
                                    account.api_key.clone(),
                                    skills.get_untracked(),
                                ));
                                ppq_account.set(Some(account.clone()));
                                start_ppq_balance_watch(
                                    PpqClient::new(),
                                    account,
                                    ppq_low_balance_usd,
                                    ppq_balance_usd,
                                    ppq_topup_in_progress,
                                    Rc::clone(&ppq_topup_grace_until),
                                    low_balance_threshold,
                                );
                                ready.set(true);
                                busy.set(false);
                                status.set("Wallet ready".to_owned());
                                push_message(
                                    &messages,
                                    assistant_message(
                                        "Fedigents is ready. Ask me to check balance, create invoices, or prepare a Lightning payment for review.",
                                    ),
                                );
                            }
                            Err(err) => {
                                busy.set(false);
                                status.set("PPQ setup needs recovery".to_owned());
                                push_message(
                                    &messages,
                                    onboarding_message(format!(
                                        "PPQ funding completed but the final ready marker could not be saved: {err}. Chat stays locked to avoid double-funding on restart."
                                    )),
                                );
                            }
                        },
                        Err(err) => {
                            busy.set(false);
                            status.set("PPQ funding failed".to_owned());
                            push_message(
                                &messages,
                                onboarding_message(format!("PPQ funding failed: {err}")),
                            );
                        }
                    }
                }
                Ok(true) => {
                    let ppq = PpqClient::new();
                    match wallet.ppq_account().await {
                        Ok(Some(account)) => {
                            agent_cell.borrow_mut().replace(WalletAgent::new(
                                wallet.clone(),
                                ppq,
                                account.api_key.clone(),
                                skills.get_untracked(),
                            ));
                            ppq_account.set(Some(account.clone()));
                            start_ppq_balance_watch(
                                PpqClient::new(),
                                account,
                                ppq_low_balance_usd,
                                ppq_balance_usd,
                                ppq_topup_in_progress,
                                Rc::clone(&ppq_topup_grace_until),
                                low_balance_threshold,
                            );
                            ready.set(true);
                            busy.set(false);
                            status.set("Wallet ready".to_owned());
                        }
                        Ok(None) => match wallet.repair_ppq_account().await {
                            Ok(account) => {
                                agent_cell.borrow_mut().replace(WalletAgent::new(
                                    wallet.clone(),
                                    ppq,
                                    account.api_key.clone(),
                                    skills.get_untracked(),
                                ));
                                ppq_account.set(Some(account.clone()));
                                start_ppq_balance_watch(
                                    PpqClient::new(),
                                    account,
                                    ppq_low_balance_usd,
                                    ppq_balance_usd,
                                    ppq_topup_in_progress,
                                    Rc::clone(&ppq_topup_grace_until),
                                    low_balance_threshold,
                                );
                                ready.set(true);
                                busy.set(false);
                                status.set("Wallet ready".to_owned());
                                push_message(
                                    &messages,
                                    onboarding_message(
                                        "PPQ account metadata was missing, so Fedigents created a replacement app-local account record without re-funding it.",
                                    ),
                                );
                            }
                            Err(err) => {
                                busy.set(false);
                                status.set("PPQ account unavailable".to_owned());
                                push_message(
                                    &messages,
                                    onboarding_message(format!("PPQ account repair failed: {err}")),
                                );
                            }
                        },
                        Err(err) => {
                            busy.set(false);
                            status.set("PPQ account unavailable".to_owned());
                            push_message(
                                &messages,
                                onboarding_message(format!("PPQ account recovery failed: {err}")),
                            );
                        }
                    }
                }
                Err(err) => {
                    busy.set(false);
                    status.set("PPQ state unavailable".to_owned());
                    push_message(
                        &messages,
                        onboarding_message(format!("Could not read PPQ state: {err}")),
                    );
                }
            }
        });
    });

    Effect::new(move |_| {
        let model_id = selected_model.get();
        let models = available_models.get();
        let is_expensive = models.iter().any(|m| m.id == model_id && m.is_expensive());
        low_balance_threshold.set(if is_expensive { 1.0 } else { PPQ_LOW_BALANCE_USD });
    });

    let submit_prompt: Rc<dyn Fn(String)> = Rc::new({
        let agent = Rc::clone(&agent);
        let runtime = Rc::clone(&runtime);
        move |text: String| {
            let trimmed = text.trim().to_owned();
            if trimmed.is_empty() {
                return;
            }
            if !ready.get_untracked() {
                push_message(
                    &messages,
                    onboarding_message(
                        "Chat unlocks after the first deposit and PPQ funding step.",
                    ),
                );
                return;
            }

            prompt.set(String::new());
            push_message(&messages, user_message(trimmed.clone()));
            let conv = conversation.get_untracked();
            let Some(agent_value) = agent.borrow().clone() else {
                push_message(
                    &messages,
                    onboarding_message("The wallet agent is not ready yet."),
                );
                return;
            };
            busy.set(true);

            let model = selected_model.get_untracked();
            let effort_raw = thinking_effort.get_untracked();
            let effort = if effort_raw.is_empty() {
                None
            } else {
                Some(effort_raw)
            };

            let runtime_cell = Rc::clone(&runtime);
            spawn_local(async move {
                match agent_value
                    .respond(&conv, &trimmed, &model, effort.as_deref())
                    .await
                {
                    Ok(response) => {
                        pending_payment.set(response.pending_payment);
                        conversation.set(response.conversation);
                        for message in response.display_messages {
                            push_message(&messages, message);
                        }
                        // Persist session
                        save_session(&StoredSession {
                            id: session_id.get_untracked(),
                            created_at: session_created_at.get_untracked(),
                            conversation: conversation.get_untracked(),
                            display_messages: messages.get_untracked(),
                        });
                        sessions_index.set(load_sessions_index());
                        // Refresh balance
                        let runtime_value = runtime_cell.borrow().clone();
                        if let Some(runtime_value) = runtime_value {
                            if let Ok(amount_sats) = runtime_value.get_balance().await {
                                balance.set(format_balance(amount_sats));
                            }
                        }
                    }
                    Err(err) => {
                        push_message(&messages, onboarding_message(format!("Agent error: {err}")))
                    }
                }
                busy.set(false);
            });
        }
    });

    // When a payment is received, notify the agent so it can do follow-up tasks.
    {
        let submit = Rc::clone(&submit_prompt);
        Effect::new(move |_| {
            if let Some(msg) = payment_received_trigger.get() {
                // Clear first to avoid re-triggering, then submit.
                payment_received_trigger.update(|v| *v = None);
                submit(msg);
            }
        });
    }

    let confirm_payment = {
        let runtime = Rc::clone(&runtime);
        move |_ev: MouseEvent| {
            if confirming_payment.get_untracked() {
                return;
            }
            let Some(proposal) = pending_payment.get_untracked() else {
                return;
            };
            let Some(runtime_value) = runtime.borrow().clone() else {
                push_message(
                    &messages,
                    onboarding_message(
                        "Wallet runtime is unavailable, so the payment could not be sent.",
                    ),
                );
                return;
            };

            confirming_payment.set(true);
            payment_result.set(None);
            spawn_local(async move {
                let pay_result = match &proposal.kind {
                    PaymentKind::Bolt11 { invoice, .. } => {
                        runtime_value.pay(invoice, None).await
                    }
                    PaymentKind::LnAddress { address, amount_sats } => {
                        runtime_value.pay(address, Some(*amount_sats)).await
                    }
                };
                match pay_result
                {
                    Ok(result) => {
                        push_message(
                            &messages,
                            ChatMessage {
                                role: ChatRole::Tool,
                                body: format!("pay_lightning => {result}"),
                            },
                        );
                        payment_result.set(Some(Ok("Payment sent successfully.".to_owned())));
                        payment_received_trigger.set(Some(format!(
                            "Outgoing payment succeeded: {}",
                            proposal.summary
                        )));
                        if let Ok(amount_sats) = runtime_value.get_balance().await {
                            balance.set(format_balance(amount_sats));
                        }
                    }
                    Err(err) => {
                        payment_result.set(Some(Err(format!("Payment failed: {err}"))));
                    }
                }
                confirming_payment.set(false);
            });
        }
    };

    let dismiss_payment = {
        move |_ev: MouseEvent| {
            if pending_payment.get_untracked().is_some() || payment_result.get_untracked().is_some() {
                pending_payment.set(None);
                payment_result.set(None);
            }
        }
    };

    let toggle_scan = move |_ev: MouseEvent| {
        scanner_open.update(|open| *open = !*open);
    };

    let top_up_ppq = {
        let runtime = Rc::clone(&runtime);
        let ppq_topup_grace_until = Rc::clone(&ppq_topup_grace_until);
        move |_ev: MouseEvent| {
            if ppq_topup_in_progress.get_untracked() {
                return;
            }
            let Some(account) = ppq_account.get_untracked() else {
                push_message(
                    &messages,
                    onboarding_message("PPQ account metadata is unavailable."),
                );
                return;
            };
            let Some(runtime_value) = runtime.borrow().clone() else {
                push_message(
                    &messages,
                    onboarding_message("Wallet runtime is unavailable."),
                );
                return;
            };

            let balance_hint = ppq_low_balance_usd.get_untracked();
            ppq_topup_in_progress.set(true);
            let ppq_topup_grace_until = Rc::clone(&ppq_topup_grace_until);
            spawn_local(async move {
                let ppq = PpqClient::new();
                let result = async {
                    let topup_usd = low_balance_threshold.get_untracked();
                    let topup = ppq.create_lightning_topup(&account, topup_usd).await?;
                    runtime_value.pay(&topup.invoice, None).await?;
                    anyhow::Ok(())
                }
                .await;

                match result {
                    Ok(()) => {
                        *ppq_topup_grace_until.borrow_mut() =
                            Some(Date::now() + PPQ_TOPUP_GRACE_MS);
                        ppq_low_balance_usd.set(None);
                        push_message(
                            &messages,
                            onboarding_message(
                                "PPQ top-up payment sent automatically. Credit balance should refresh shortly.",
                            ),
                        );
                        if let Ok(amount_sats) = runtime_value.get_balance().await {
                            balance.set(format_balance(amount_sats));
                        }
                    }
                    Err(err) => {
                        ppq_low_balance_usd.set(balance_hint.or(Some(0.0)));
                        push_message(
                            &messages,
                            onboarding_message(format!("PPQ top-up failed: {err}")),
                        );
                    }
                }

                ppq_topup_in_progress.set(false);
            });
        }
    };

    let recording = RwSignal::new(false);
    let transcribing = RwSignal::new(false);
    let rec_drag_x = RwSignal::new(0.0_f64);
    let rec_start_x = RwSignal::new(0.0_f64);
    let rec_cancelled = RwSignal::new(false);
    let rec_bar_ref = NodeRef::<leptos::html::Div>::new();
    let scan_submit = Rc::clone(&submit_prompt);
    let form_submit = Rc::clone(&submit_prompt);
    let key_submit = Rc::clone(&submit_prompt);
    let voice_submit = Rc::clone(&submit_prompt);

    view! {
        <div class="shell">
            <div class="wallet-frame">
                <div
                    class="ppq-topup-banner"
                    style:display=move || if ppq_low_balance_usd.get().is_some() { "grid" } else { "none" }
                >
                    <div class="ppq-topup-copy">
                        <div class="ppq-topup-title">"Low PPQ balance"</div>
                        <div class="ppq-topup-body">
                            {move || ppq_low_balance_usd.get().map(|usd| {
                                let topup = low_balance_threshold.get();
                                format!("PPQ credit is down to ${usd:.2}. Top up ${topup:.2} now?")
                            }).unwrap_or_default()}
                        </div>
                    </div>
                    <button
                        class="action-button"
                        type="button"
                        on:click=top_up_ppq
                        disabled=move || ppq_topup_in_progress.get()
                    >
                        {move || if ppq_topup_in_progress.get() {
                            "Topping up...".to_owned()
                        } else {
                            let topup = low_balance_threshold.get();
                            if topup < 1.0 {
                                format!("Top up {:.0}c", topup * 100.0)
                            } else {
                                format!("Top up ${topup:.2}")
                            }
                        }}
                    </button>
                </div>
                <header class="topbar">
                    <div class="menu-wrapper">
                        <button class="menu-button" title="Menu" on:click=move |_| menu_open.update(|v| *v = !*v)>
                            <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <line x1="3" y1="6" x2="21" y2="6"/>
                                <line x1="3" y1="12" x2="21" y2="12"/>
                                <line x1="3" y1="18" x2="21" y2="18"/>
                            </svg>
                        </button>
                        {move || menu_open.get().then(|| {
                            let load_session_handler = move |selected: String| {
                                if let Some(session) = load_session(&selected) {
                                    save_session(&StoredSession {
                                        id: session_id.get_untracked(),
                                        created_at: session_created_at.get_untracked(),
                                        conversation: conversation.get_untracked(),
                                        display_messages: messages.get_untracked(),
                                    });
                                    session_id.set(session.id);
                                    session_created_at.set(session.created_at);
                                    conversation.set(session.conversation);
                                    messages.set(session.display_messages);
                                    pending_payment.set(None);
                                    menu_open.set(false);
                                }
                            };
                            view! {
                                <div class="menu-backdrop" on:click=move |_| menu_open.set(false)/>
                                <div class="menu-panel">
                                    <button class="new-session-button" on:click=move |_| {
                                        save_session(&StoredSession {
                                            id: session_id.get_untracked(),
                                            created_at: session_created_at.get_untracked(),
                                            conversation: conversation.get_untracked(),
                                            display_messages: messages.get_untracked(),
                                        });
                                        let new_id = format!("fedigents.session.{}", Date::now() as u64);
                                        session_id.set(new_id);
                                        session_created_at.set(Date::now());
                                        conversation.set(ConversationLog::new());
                                        messages.set(vec![onboarding_message("New session started.")]);
                                        pending_payment.set(None);
                                        sessions_index.set(load_sessions_index());
                                        menu_open.set(false);
                                    }>"+ New session"</button>
                                    <div class="menu-section-label">"Past Sessions"</div>
                                    <div class="session-list">
                                        {move || sessions_index.get().into_iter().rev().map(|entry| {
                                            let id = entry.id.clone();
                                            let label = entry.preview.clone();
                                            let handler = load_session_handler.clone();
                                            view! {
                                                <button class="session-item" on:click=move |_| handler(id.clone())>
                                                    {label}
                                                </button>
                                            }
                                        }).collect_view()}
                                    </div>
                                    <div class="menu-divider"/>
                                    <label class="debug-toggle">
                                        <input type="checkbox"
                                            prop:checked=move || debug_mode.get()
                                            on:change=move |_| debug_mode.update(|v| *v = !*v)
                                        />
                                        "Debug"
                                    </label>
                                    <div class="menu-divider"/>
                                    <div class="ppq-balance-row">
                                        <span class="menu-section-label">"PPQ Balance"</span>
                                        <span class="ppq-balance-value">{move || match ppq_balance_usd.get() {
                                            Some(usd) => format!("${:.4}", usd),
                                            None => "—".to_owned(),
                                        }}</span>
                                    </div>
                                    <div class="menu-divider"/>
                                    <div class="menu-section-label">"Model"</div>
                                    <select class="model-select"
                                        prop:value=move || selected_model.get()
                                        on:change=move |ev| {
                                            let val = event_target_value(&ev);
                                            selected_model.set(val.clone());
                                            let _: Result<(), _> = LocalStorage::set(MODEL_STORAGE_KEY, &val);
                                        }
                                    >
                                        {move || {
                                            let models = available_models.get();
                                            let current = selected_model.get();
                                            let mut popular: Vec<_> = models.iter().filter(|m| m.popular).cloned().collect();
                                            let mut others: Vec<_> = models.iter().filter(|m| !m.popular).cloned().collect();
                                            if !popular.iter().any(|m| m.id == current) && !others.iter().any(|m| m.id == current) {
                                                others.insert(0, PpqModel { id: current.clone(), name: current.clone(), ..Default::default() });
                                            }
                                            popular.sort_by(|a, b| a.pricing.output_per_1M_tokens.total_cmp(&b.pricing.output_per_1M_tokens));
                                            others.sort_by(|a, b| a.pricing.output_per_1M_tokens.total_cmp(&b.pricing.output_per_1M_tokens));
                                            view! {
                                                <optgroup label="Popular">
                                                    {popular.into_iter().map(|m| {
                                                        let sel = m.id == current;
                                                        let label = format_model_label(&m);
                                                        view! { <option value={m.id} selected=sel>{label}</option> }
                                                    }).collect_view()}
                                                </optgroup>
                                                <optgroup label="All models">
                                                    {others.into_iter().map(|m| {
                                                        let sel = m.id == current;
                                                        let label = format_model_label(&m);
                                                        view! { <option value={m.id} selected=sel>{label}</option> }
                                                    }).collect_view()}
                                                </optgroup>
                                            }
                                        }}
                                    </select>
                                    <div class="thinking-effort">
                                        <span class="thinking-effort-label">"Thinking: " {move || match thinking_effort.get().as_str() {
                                            "low" => "Low",
                                            "medium" => "Medium",
                                            "high" => "High",
                                            _ => "Off",
                                        }}</span>
                                        <input type="range" min="0" max="3" step="1"
                                            prop:value=move || match thinking_effort.get().as_str() {
                                                "low" => "1",
                                                "medium" => "2",
                                                "high" => "3",
                                                _ => "0",
                                            }
                                            on:input=move |ev| {
                                                let val = event_target_value(&ev);
                                                let effort = match val.as_str() {
                                                    "1" => "low",
                                                    "2" => "medium",
                                                    "3" => "high",
                                                    _ => "",
                                                };
                                                thinking_effort.set(effort.to_owned());
                                                let _: Result<(), _> = LocalStorage::set(THINKING_EFFORT_KEY, effort);
                                            }
                                        />
                                    </div>
                                </div>
                            }
                        })}
                    </div>

                    <div class="balance-card">
                        <div class="balance-label">"Balance"</div>
                        <div class="balance-value">{move || balance.get()}</div>
                        <div class="meta-text">{move || status.get()}</div>
                    </div>

                    <button class="scan-button" on:click=toggle_scan disabled=move || busy.get() title="Scan QR">
                        <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                            <rect x="2" y="2" width="8" height="8" rx="1"/>
                            <rect x="14" y="2" width="8" height="8" rx="1"/>
                            <rect x="2" y="14" width="8" height="8" rx="1"/>
                            <rect x="14" y="14" width="4" height="4" rx="0.5"/>
                            <line x1="22" y1="14" x2="22" y2="22"/>
                            <line x1="14" y1="22" x2="22" y2="22"/>
                            <rect x="5" y="5" width="2" height="2"/>
                            <rect x="17" y="5" width="2" height="2"/>
                            <rect x="5" y="17" width="2" height="2"/>
                        </svg>
                    </button>
                </header>

                <section class="chat-panel">
                    <div class="chat-history">
                        {move || {
                            let receive = receive_code.get();
                            let show_receive = receive.is_some() && !ready.get();
                            let receive_view = show_receive.then(|| {
                                let lnurl = receive.unwrap_or_default();
                                let qr_svg = generate_qr_svg(&lnurl);
                                let lnurl_for_copy = lnurl.clone();
                                view! {
                                    <div class="receive-card">
                                        <div class="message-role">"First deposit"</div>
                                        <div>
                                            "Use this payment code to fund the wallet. After the first receive settles, Fedigents will top up "
                                            <a href="https://ppq.ai" target="_blank" rel="noopener noreferrer">"PPQ.ai"</a>
                                            " with $0.10 and unlock the chat interface."
                                        </div>
                                        {qr_svg.map(|svg| view! {
                                            <div class="qr-inline">
                                                <div class="qr-svg" inner_html=svg></div>
                                            </div>
                                        })}
                                        <div class="receive-code">{lnurl.clone()}</div>
                                        <div>
                                            <button class="secondary-button" on:click=move |_| {
                                                let lnurl = lnurl_for_copy.clone();
                                                spawn_local(async move {
                                                    let _ = browser::copy_to_clipboard(&lnurl).await;
                                                });
                                            }>
                                                "Copy"
                                            </button>
                                        </div>
                                    </div>
                                }
                            });

                            let show_debug = debug_mode.get();
                            let chat_nodes = messages
                                .get()
                                .into_iter()
                                .filter(|m| show_debug || matches!(m.role, ChatRole::User | ChatRole::Assistant))
                                .map(|m| render_message(m, paid_invoices))
                                .collect_view();

                            view! {
                                {receive_view}
                                {chat_nodes}
                            }
                        }}
                        <div
                            class="transcribing-indicator"
                            style:display=move || if transcribing.get() { "flex" } else { "none" }
                        >
                            <span class="typing-dot"></span>
                            <span class="typing-dot"></span>
                            <span class="typing-dot"></span>
                        </div>
                        <div
                            class="typing-indicator"
                            style:display=move || if busy.get() && ready.get() { "flex" } else { "none" }
                        >
                            <span class="typing-dot"></span>
                            <span class="typing-dot"></span>
                            <span class="typing-dot"></span>
                        </div>
                        <article
                            class="pending-payment-card"
                            style:display=move || if pending_payment.get().is_some() || confirming_payment.get() || payment_result.get().is_some() { "grid" } else { "none" }
                        >
                            <div class="payment-result-area"
                                style:display=move || if payment_result.get().is_some() { "flex" } else { "none" }
                                class:payment-success=move || matches!(payment_result.get(), Some(Ok(_)))
                                class:payment-error=move || matches!(payment_result.get(), Some(Err(_)))
                            >
                                <span>{move || payment_result.get().map(|r| match r { Ok(m) | Err(m) => m }).unwrap_or_default()}</span>
                                <button class="secondary-button" type="button" on:click=dismiss_payment>"Dismiss"</button>
                            </div>
                            <div class="payment-form-area"
                                style:display=move || if payment_result.get().is_none() { "grid" } else { "none" }
                            >
                                <div class="message-meta">
                                    <span class="message-role">"pending payment"</span>
                                </div>
                                <div class="pending-payment-summary">
                                    {move || pending_payment.get().map(|proposal| proposal.summary).unwrap_or_default()}
                                </div>
                                <dl class="pending-payment-details">
                                    <div>
                                        <dt>"Amount"</dt>
                                        <dd>
                                            {move || pending_payment.get().map(|proposal| match &proposal.kind {
                                                PaymentKind::Bolt11 { amount_sats: Some(amount), .. } => format!("{amount} sats"),
                                                PaymentKind::LnAddress { amount_sats, .. } => format!("{amount_sats} sats"),
                                                _ => "Amount encoded in invoice".to_owned(),
                                            }).unwrap_or_default()}
                                        </dd>
                                    </div>
                                    <div>
                                        <dt>"Request"</dt>
                                        <dd class="payment-request">
                                            {move || pending_payment.get().map(|proposal| match &proposal.kind {
                                                PaymentKind::Bolt11 { invoice, .. } => truncate_middle(invoice, 96),
                                                PaymentKind::LnAddress { address, .. } => address.clone(),
                                            }).unwrap_or_default()}
                                        </dd>
                                    </div>
                                </dl>
                                <div class="payment-actions">
                                    <button class="action-button" type="button" on:click=confirm_payment disabled=move || confirming_payment.get()>
                                        {move || if confirming_payment.get() { "Sending..." } else { "Confirm payment" }}
                                    </button>
                                    <button class="secondary-button" type="button" on:click=dismiss_payment disabled=move || confirming_payment.get()>
                                        "Cancel"
                                    </button>
                                </div>
                            </div>
                        </article>
                    </div>
                </section>

                <footer class="composer">
                    <form on:submit=move |ev: SubmitEvent| {
                        ev.prevent_default();
                        form_submit(prompt.get_untracked());
                    }>
                        <textarea
                            node_ref=textarea_ref
                            prop:value=move || prompt.get()
                            on:input=move |ev| prompt.set(event_target_value(&ev))
                            on:keydown=move |ev: KeyboardEvent| {
                                if ev.key() == "Enter" && !ev.shift_key() {
                                    ev.prevent_default();
                                    key_submit(prompt.get_untracked());
                                }
                            }
                            rows="1"
                            placeholder="Send a message..."
                            disabled=move || !ready.get() || busy.get()
                        ></textarea>
                        <button type="submit" class="send-button"
                            style:display=move || if prompt.get().trim().is_empty() { "none" } else { "flex" }
                            disabled=move || !ready.get() || busy.get()
                        >
                            <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
                                <line x1="12" y1="19" x2="12" y2="5"/>
                                <polyline points="5 12 12 5 19 12"/>
                            </svg>
                        </button>
                        <button
                            type="button"
                            class="mic-button"
                            style:display=move || if prompt.get().trim().is_empty() && !recording.get() { "flex" } else { "none" }
                            disabled=move || !ready.get() || busy.get() || transcribing.get()
                            on:pointerdown=move |ev: web_sys::PointerEvent| {
                                ev.prevent_default();
                                rec_start_x.set(ev.client_x() as f64);
                                rec_drag_x.set(0.0);
                                rec_cancelled.set(false);
                                recording.set(true);
                                // Capture the pointer on the recording-bar so
                                // pointerup/pointermove fire there even though
                                // the original target (mic-button) will be
                                // hidden.  Without this, mobile browsers either
                                // swallow the pointerup or fire pointercancel
                                // when the touch target disappears.
                                if let Some(bar) = rec_bar_ref.get_untracked() {
                                    let el: &web_sys::Element = &bar;
                                    let _ = el.set_pointer_capture(ev.pointer_id());
                                }
                                spawn_local(async move {
                                    if let Err(e) = browser::begin_recording().await {
                                        leptos::logging::warn!("Failed to start recording: {e}");
                                        recording.set(false);
                                    }
                                });
                            }
                        >
                            <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <rect x="9" y="1" width="6" height="14" rx="3"/>
                                <path d="M19 10v2a7 7 0 0 1-14 0v-2"/>
                                <line x1="12" y1="19" x2="12" y2="23"/>
                                <line x1="8" y1="23" x2="16" y2="23"/>
                            </svg>
                        </button>
                    </form>
                    <div
                        node_ref=rec_bar_ref
                        class="recording-bar"
                        class:over-trash=move || rec_drag_x.get() < -30.0
                        style:display=move || if recording.get() { "flex" } else { "none" }
                        on:pointermove=move |ev: web_sys::PointerEvent| {
                            if !recording.get_untracked() { return; }
                            let dx = ev.client_x() as f64 - rec_start_x.get_untracked();
                            rec_drag_x.set(dx.clamp(-40.0, 0.0));
                        }
                        on:pointerup={
                            let voice_submit = voice_submit.clone();
                            move |_| {
                                if !recording.get_untracked() { return; }
                                let cancelled = rec_drag_x.get_untracked() < -30.0;
                                recording.set(false);
                                rec_drag_x.set(0.0);
                                if cancelled {
                                    spawn_local(async move {
                                        browser::cancel_recording().await;
                                    });
                                } else {
                                    let submit = voice_submit.clone();
                                    let api_key = ppq_account.get_untracked().map(|a| a.api_key.clone()).unwrap_or_default();
                                    transcribing.set(true);
                                    spawn_local(async move {
                                        match browser::finish_recording_and_transcribe(&api_key).await {
                                            Ok(text) if !text.trim().is_empty() => submit(text),
                                            Ok(_) => leptos::logging::warn!("Transcription returned empty text"),
                                            Err(e) => leptos::logging::warn!("Transcription failed: {e}"),
                                        }
                                        transcribing.set(false);
                                    });
                                }
                            }
                        }
                        on:pointercancel=move |_| {
                            if !recording.get_untracked() { return; }
                            recording.set(false);
                            rec_drag_x.set(0.0);
                            spawn_local(async move {
                                browser::cancel_recording().await;
                            });
                        }
                    >
                        <span class="rec-dot"></span>
                        <span class="rec-label">"< Slide to cancel"</span>
                        <div class="trash-zone">
                            <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <polyline points="3 6 5 6 21 6"/>
                                <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>
                            </svg>
                        </div>
                        <div class="rec-mic-icon" style:transform=move || format!("translateX({}px)", rec_drag_x.get())>
                            <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <rect x="9" y="1" width="6" height="14" rx="3"/>
                                <path d="M19 10v2a7 7 0 0 1-14 0v-2"/>
                                <line x1="12" y1="19" x2="12" y2="23"/>
                                <line x1="8" y1="23" x2="16" y2="23"/>
                            </svg>
                        </div>
                    </div>
                </footer>

                <div style:display=move || if scanner_open.get() { "flex" } else { "none" } class="scanner-overlay" on:click=move |_| scanner_open.set(false)>
                    <div class="scanner-card" on:click=move |ev: MouseEvent| ev.stop_propagation()>
                        <Scan
                            active=scanner_open
                            on_scan=move |data: String| {
                                scanner_open.set(false);
                                scan_submit(data);
                            }
                            class=""
                            video_class="scanner-video"
                        />
                        <button class="secondary-button" on:click=move |_| scanner_open.set(false)>
                            "Close scanner"
                        </button>
                    </div>
                </div>
            </div>
        </div>
    }
}

fn render_message(message: ChatMessage, paid_invoices: RwSignal<HashSet<String>>) -> impl IntoView {
    let role_class = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    };
    let role_label = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "you",
        ChatRole::Assistant => "agent",
        ChatRole::Tool => "tool",
    };

    let (clean_body, invoices) = extract_invoice_tags(&message.body);
    let html_body = markdown_to_html(&clean_body);

    view! {
        <article class=format!("message {role_class}")>
            <div class="message-meta">
                <span class="message-role">{role_label}</span>
            </div>
            <div class="message-body markdown-body" inner_html=html_body></div>
            {invoices.into_iter().map(|inv| render_invoice_widget(inv, paid_invoices)).collect_view()}
        </article>
    }
}

fn render_invoice_widget(invoice: String, paid_invoices: RwSignal<HashSet<String>>) -> impl IntoView {
    let truncated = truncate_middle(&invoice, 32);
    let qr_visible = RwSignal::new(false);
    let qr_svg = generate_qr_svg(&invoice);
    let invoice_for_copy = invoice.clone();
    let invoice_for_paid = invoice.clone();

    view! {
        <div class="invoice-widget">
            <div class="invoice-header">
                <code class="invoice-abbrev">{truncated}</code>
                {move || paid_invoices.get().contains(&invoice_for_paid).then(|| view! {
                    <span class="invoice-paid-badge">"Paid"</span>
                })}
            </div>
            <div class="invoice-actions">
                <button class="secondary-button" on:click=move |_| {
                    let d = invoice_for_copy.clone();
                    spawn_local(async move {
                        let _ = browser::copy_to_clipboard(&d).await;
                    });
                }>"Copy"</button>
                {qr_svg.is_some().then(|| view! {
                    <button class="secondary-button" on:click=move |_| qr_visible.update(|v| *v = !*v)>
                        {move || if qr_visible.get() { "Hide QR" } else { "Show QR" }}
                    </button>
                })}
            </div>
            {qr_svg.map(|svg| view! {
                <div class="qr-inline" style:display=move || if qr_visible.get() { "flex" } else { "none" }>
                    <div class="qr-svg" inner_html=svg></div>
                </div>
            })}
        </div>
    }
}

/// Extract `<invoice>...</invoice>` tags from the message body, returning
/// the cleaned text and the list of invoice strings.
fn extract_invoice_tags(text: &str) -> (String, Vec<String>) {
    let mut clean = String::with_capacity(text.len());
    let mut invoices = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<invoice>") {
        clean.push_str(&rest[..start]);
        let after_tag = &rest[start + "<invoice>".len()..];
        if let Some(end) = after_tag.find("</invoice>") {
            let inv = after_tag[..end].trim().to_owned();
            if !inv.is_empty() {
                invoices.push(inv);
            }
            rest = &after_tag[end + "</invoice>".len()..];
        } else {
            // No closing tag — keep the rest as-is
            clean.push_str(&rest[start..]);
            rest = "";
            break;
        }
    }
    clean.push_str(rest);
    (clean, invoices)
}

fn format_balance(sats: u64) -> String {
    format!("{sats} sats")
}

fn push_message(messages: &RwSignal<Vec<ChatMessage>>, message: ChatMessage) {
    messages.update(|items| items.push(message));
}

fn truncate_middle(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }

    let head = limit / 2;
    let tail = limit.saturating_sub(head + 1);
    let start = value.chars().take(head).collect::<String>();
    let end = value
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{start}...{end}")
}

async fn fund_ppq(wallet: &WalletRuntime, ppq: &PpqClient) -> anyhow::Result<PpqAccount> {
    let account = wallet.ensure_ppq_account().await?;
    let topup = ppq.create_lightning_topup(&account, PPQ_TOPUP_USD).await?;
    wallet.begin_ppq_funding_attempt().await?;
    wallet.pay(&topup.invoice, None).await?;
    Ok(account)
}

fn start_ppq_balance_watch(
    ppq: PpqClient,
    account: PpqAccount,
    ppq_low_balance_usd: RwSignal<Option<f64>>,
    ppq_balance_usd: RwSignal<Option<f64>>,
    ppq_topup_in_progress: RwSignal<bool>,
    ppq_topup_grace_until: Rc<RefCell<Option<f64>>>,
    low_balance_threshold: RwSignal<f64>,
) {
    spawn_local(async move {
        loop {
            match ppq.balance(&account).await {
                Ok(balance) => {
                    ppq_balance_usd.set(Some(balance.amount_usd));
                    let threshold = low_balance_threshold.get_untracked();
                    let in_grace = ppq_topup_grace_until
                        .borrow()
                        .is_some_and(|until| until > Date::now());
                    if balance.amount_usd >= threshold {
                        *ppq_topup_grace_until.borrow_mut() = None;
                        ppq_low_balance_usd.set(None);
                    } else if !ppq_topup_in_progress.get_untracked() && !in_grace {
                        ppq_low_balance_usd.set(Some(balance.amount_usd));
                    }
                }
                Err(err) => {
                    tracing::warn!("PPQ balance check failed: {err}");
                }
            }

            sleep(PPQ_BALANCE_POLL_INTERVAL).await;
        }
    });
}

fn markdown_to_html(input: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(input, options);
    let mut output = String::new();
    html::push_html(&mut output, parser);
    output
}

fn generate_qr_svg(data: &str) -> Option<String> {
    use qrcode::render::svg;
    use qrcode::QrCode;
    let code = QrCode::new(data.as_bytes()).ok()?;
    Some(
        code.render::<svg::Color>()
            .min_dimensions(180, 180)
            .max_dimensions(180, 180)
            .dark_color(svg::Color("#0b1013"))
            .light_color(svg::Color("#ffffff"))
            .build(),
    )
}


fn format_model_label(model: &PpqModel) -> String {
    let out = model.pricing.output_per_1M_tokens;
    if out <= 0.0 {
        model.name.clone()
    } else {
        format!("{} (${:.2}/M out)", model.name, out)
    }
}

fn event_target_value(ev: &web_sys::Event) -> String {
    let target = match ev.target() {
        Some(t) => t,
        None => return String::new(),
    };
    if let Ok(el) = target.clone().dyn_into::<HtmlTextAreaElement>() {
        return el.value();
    }
    if let Ok(el) = target.clone().dyn_into::<web_sys::HtmlSelectElement>() {
        return el.value();
    }
    if let Ok(el) = target.dyn_into::<web_sys::HtmlInputElement>() {
        return el.value();
    }
    String::new()
}
