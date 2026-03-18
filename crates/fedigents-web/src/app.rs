use std::cell::RefCell;
use std::rc::Rc;

use leptos::ev::{KeyboardEvent, MouseEvent, SubmitEvent};
use leptos::html::Textarea;
use leptos::prelude::*;
use leptos_qr_scanner::Scan;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlTextAreaElement;

use crate::agent::{
    assistant_message, load_skills, onboarding_message, user_message, ChatMessage, ChatRole,
    PendingPaymentProposal, SkillSummary, WalletAgent,
};
use crate::browser;
use crate::ppq::PpqClient;
use crate::wallet_runtime::{BootstrapEvent, OperationEvent, WalletRuntime};

const PPQ_TOPUP_USD: f64 = 0.10;

#[component]
pub fn App() -> impl IntoView {
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
    let scanner_open = RwSignal::new(false);
    let debug_mode = RwSignal::new(false);
    let textarea_ref = NodeRef::<Textarea>::new();

    let effect_runtime = Rc::clone(&runtime);
    let effect_agent = Rc::clone(&agent);
    Effect::new(move |_| {
        let runtime_cell = Rc::clone(&effect_runtime);
        let agent_cell = Rc::clone(&effect_agent);
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
                wallet.set_operation_listener(Some(Rc::new(move |event| {
                    match event {
                        OperationEvent::PaymentReceived { amount_sats } => {
                            let msg = match amount_sats {
                                Some(sats) => format!("Incoming payment of {sats} sats received."),
                                None => "Incoming payment received.".to_owned(),
                            };
                            push_message(&messages, onboarding_message(msg));
                            let wallet = listener_wallet.clone();
                            spawn_local(async move {
                                if let Ok(sats) = wallet.get_balance().await {
                                    balance.set(format_balance(sats));
                                }
                            });
                        }
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
                        Ok(api_key) => match wallet.mark_ppq_ready().await {
                            Ok(()) => {
                                agent_cell.borrow_mut().replace(WalletAgent::new(
                                    wallet.clone(),
                                    ppq,
                                    api_key,
                                    skills.get_untracked(),
                                ));
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
                                account.api_key,
                                skills.get_untracked(),
                            ));
                            ready.set(true);
                            busy.set(false);
                            status.set("Wallet ready".to_owned());
                        }
                        Ok(None) => match wallet.repair_ppq_account().await {
                            Ok(account) => {
                                agent_cell.borrow_mut().replace(WalletAgent::new(
                                    wallet.clone(),
                                    ppq,
                                    account.api_key,
                                    skills.get_untracked(),
                                ));
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
            let history = messages.get_untracked();
            let Some(agent_value) = agent.borrow().clone() else {
                push_message(
                    &messages,
                    onboarding_message("The wallet agent is not ready yet."),
                );
                return;
            };
            busy.set(true);

            let runtime_cell = Rc::clone(&runtime);
            spawn_local(async move {
                match agent_value.respond(&history, &trimmed).await {
                    Ok(response) => {
                        pending_payment.set(response.pending_payment);
                        for message in response.messages {
                            push_message(&messages, message);
                        }
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
            busy.set(true);
            pending_payment.set(None);
            spawn_local(async move {
                match runtime_value
                    .pay(&proposal.payment, proposal.amount_sats)
                    .await
                {
                    Ok(result) => {
                        push_message(
                            &messages,
                            ChatMessage {
                                role: ChatRole::Tool,
                                body: format!("pay_lightning => {result}"),
                            },
                        );
                        push_message(&messages, assistant_message("Payment sent."));
                        if let Ok(amount_sats) = runtime_value.get_balance().await {
                            balance.set(format_balance(amount_sats));
                        }
                    }
                    Err(err) => {
                        pending_payment.set(Some(proposal));
                        push_message(
                            &messages,
                            onboarding_message(format!("Payment failed: {err}")),
                        );
                    }
                }
                confirming_payment.set(false);
                busy.set(false);
            });
        }
    };

    let dismiss_payment = {
        move |_ev: MouseEvent| {
            if let Some(proposal) = pending_payment.get_untracked() {
                pending_payment.set(None);
                push_message(
                    &messages,
                    onboarding_message(format!("Dismissed pending payment: {}", proposal.summary)),
                );
            }
        }
    };

    let toggle_scan = move |_ev: MouseEvent| {
        scanner_open.update(|open| *open = !*open);
    };

    let scan_submit = Rc::clone(&submit_prompt);
    let form_submit = Rc::clone(&submit_prompt);
    let key_submit = Rc::clone(&submit_prompt);

    view! {
        <div class="shell">
            <div class="wallet-frame">
                <header class="topbar">
                    <div class="balance-card">
                        <div class="balance-label">"Balance"</div>
                        <div class="balance-value">{move || balance.get()}</div>
                        <div class="meta-text">{move || status.get()}</div>
                    </div>

                    <label class="topbar-center">
                        <input type="checkbox"
                            prop:checked=move || debug_mode.get()
                            on:change=move |_| debug_mode.update(|v| *v = !*v)
                        />
                        "Debug"
                    </label>

                    <button class="scan-button" on:click=toggle_scan disabled=move || busy.get()>
                        <span>"Scan QR"</span>
                        <span aria-hidden="true">"[]"</span>
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
                                            "Use this LNURL to fund the wallet. After the first receive settles, Fedigents will top up PPQ with $0.10 and unlock the chat interface."
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
                                                "Copy LNURL"
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
                                .map(render_message)
                                .collect_view();

                            view! {
                                {receive_view}
                                {chat_nodes}
                            }
                        }}
                        <article
                            class="pending-payment-card"
                            style:display=move || if pending_payment.get().is_some() { "grid" } else { "none" }
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
                                        {move || pending_payment.get().and_then(|proposal| proposal.amount_sats).map(|amount| format!("{amount} sats")).unwrap_or_else(|| "Amount comes from the request".to_owned())}
                                    </dd>
                                </div>
                                <div>
                                    <dt>"Request"</dt>
                                    <dd class="payment-request">
                                        {move || pending_payment.get().map(|proposal| truncate_middle(&proposal.payment, 96)).unwrap_or_default()}
                                    </dd>
                                </div>
                            </dl>
                            <button class="action-button" type="button" on:click=confirm_payment disabled=move || busy.get() || confirming_payment.get()>
                                {move || if busy.get() || confirming_payment.get() { "Sending..." } else { "Confirm payment" }}
                            </button>
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
                        <button type="submit" disabled=move || !ready.get() || busy.get()></button>
                    </form>
                    <div
                        class="supporting"
                        style:display=move || if pending_payment.get().is_some() { "flex" } else { "none" }
                    >
                        <button
                            class="secondary-button"
                            type="button"
                            on:click=dismiss_payment
                            disabled=move || busy.get()
                        >
                            "Dismiss pending payment"
                        </button>
                    </div>
                </footer>
            </div>

            <div style:display=move || if scanner_open.get() { "grid" } else { "none" } class="modal-shell">
                <Scan
                    active=scanner_open
                    on_scan=move |data: String| {
                        scanner_open.set(false);
                        scan_submit(data);
                    }
                    class=""
                    video_class="scanner-video"
                />
            </div>
        </div>
    }
}

fn render_message(message: ChatMessage) -> impl IntoView {
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

    let html_body = markdown_to_html(&message.body);
    let qr_codes = extract_payable_strings(&message.body)
        .into_iter()
        .filter_map(|s| generate_qr_svg(&s).map(|svg| (s, svg)))
        .collect::<Vec<_>>();

    view! {
        <article class=format!("message {role_class}")>
            <div class="message-meta">
                <span class="message-role">{role_label}</span>
            </div>
            <div class="message-body markdown-body" inner_html=html_body></div>
            {qr_codes.into_iter().map(|(data, svg)| {
                let data_for_copy = data.clone();
                view! {
                    <div class="qr-inline">
                        <div class="qr-svg" inner_html=svg></div>
                        <button class="secondary-button qr-copy-btn" on:click=move |_| {
                            let d = data_for_copy.clone();
                            spawn_local(async move {
                                let _ = browser::copy_to_clipboard(&d).await;
                            });
                        }>"Copy"</button>
                    </div>
                }
            }).collect_view()}
        </article>
    }
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

async fn fund_ppq(wallet: &WalletRuntime, ppq: &PpqClient) -> anyhow::Result<String> {
    let account = wallet.ensure_ppq_account().await?;
    let topup = ppq.create_lightning_topup(&account, PPQ_TOPUP_USD).await?;
    wallet.begin_ppq_funding_attempt().await?;
    wallet.pay(&topup.invoice, None).await?;
    Ok(account.api_key)
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

fn extract_payable_strings(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || c == '`' || c == '\'' || c == '"') {
        let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric());
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if lower.starts_with("lnbc")
            || lower.starts_with("lnurl")
            || lower.starts_with("lightning:")
            || lower.starts_with("bitcoin:")
        {
            if trimmed.len() > 20 {
                results.push(trimmed.to_owned());
            }
        }
    }
    results.dedup();
    results
}

fn event_target_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|target| target.dyn_into::<HtmlTextAreaElement>().ok())
        .map(|input: HtmlTextAreaElement| input.value())
        .unwrap_or_default()
}
