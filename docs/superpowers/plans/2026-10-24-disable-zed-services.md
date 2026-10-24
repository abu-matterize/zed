# No-Zed-Services Switch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One setting, `disable_zed_services: true`, after which Zed never initiates a request to a Zed-owned host, with no failed requests, retries, or user-visible errors.

**Architecture:** Nothing initiates. Every background loop and provider that would phone home is not started when the setting is on, so the code path does not exist at runtime instead of existing and erroring. A single silent backstop in `ReqwestClient::send` catches any path we missed: it drops the request and writes one debug log line, never a user-visible error.

**Tech Stack:** Rust, GPUI, `settings` + `settings_content` crates, `http_client` / `reqwest_client`, GitHub Actions on a fork.

**Spec:** This file. Design agreed in session 2026-10-24.

## What the codebase already does for us

Verified by reading the code, and it removes five tasks from the first draft:

- **No update polling on a source build.** `crates/zed/RELEASE_CHANNEL` is `dev`, and `ReleaseChannel::poll_for_updates()` returns `false` for `Dev` (`crates/release_channel/src/lib.rs:201`). `auto_update::init` only calls `start_polling` when that is true (`crates/auto_update/src/auto_update.rs:279-290`).
- **No crash upload on a source build.** `should_install_crash_handler` requires a non-`Dev` channel (`crates/client/src/telemetry.rs:98`), so no Sentry handler and no minidump uploads exist.
- **Minidump upload is already gated on a setting.** `crates/zed/src/reliability.rs:51` only uploads when `client.telemetry().diagnostics_enabled()`.
- **Telemetry flush only starts when an event is queued.** The 5-minute timer is created inside `report_event` (`crates/client/src/telemetry.rs:581-586`), so one early return stops queueing and the timer together. The two quit-time flushes (`crates/zed/src/main.rs:846`, `crates/zed/src/reliability/hang_detection.rs:79`) become no-ops on an empty queue.
- **Collab does not connect at startup.** `sign_in_with_optional_connect` says "Only Zed staff automatically connect to Collab" (`crates/client/src/client.rs:1040`), and `connect` requires credentials first (`:1091`). Signed out, nothing connects.
- **Feature flags are local settings now**, not a network fetch (`crates/feature_flags/src/store.rs:79`).

What remains genuinely phone-home on a signed-out source build: the telemetry queue, and the extension registry calls that run at startup.

## Global Constraints

- Setting name is exactly `disable_zed_services`, boolean, default `false`, top-level in `settings.json` (flattened project settings, same namespace as `disable_ai`).
- No Settings UI page. This patch is not being upstreamed, so the repo's feature checklist does not apply.
- Do not edit `.github/workflows/release.yml`, `run_tests.yml`, or any file with the header "Generated from xtask::workflows". They are regenerated and owner-gated. The fork gets its own workflow file.
- Commit subjects follow Zed's style, `crate: Capitalized sentence`. This repo has no commitlint config.
- Skip the `README.md` `> [!IMPORTANT]` prelude from `zed/AGENTS.md`. It exists for upstream PR review, and this ships as a patch. Revisit only if a PR is ever opened.

## Review Focus

1. **First request before settings load.** The flag must be set after `settings::init` and before `Client::production`. Check both line numbers before editing.
2. **Extension panel with the registry off.** A user who opens Extensions must see a reason, not an empty list that looks broken.
3. **Model and edit-prediction pickers.** Zed and Mercury must not appear, so no "sign in to use" prompt appears.
4. **Remote development.** `remote_server` and the SSH client must keep working; they do not use Zed hosts.
5. **Setting toggled at runtime.** Flipping it on and off must not leave a half-started loop.

---

### Task 1: Add the setting and its default

**Files:**
- Modify: `crates/settings_content/src/project.rs:87` (next to `disable_ai`)
- Modify: `crates/client/src/client.rs:540` (next to `TelemetrySettings`)
- Modify: `assets/settings/default.json`
- Test: `crates/client/src/client.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `ProjectSettingsContent::disable_zed_services: Option<bool>`; `client::ZedServicesSettings { pub disable_zed_services: bool }` with `get_global`.

- [ ] **Step 1: Write the failing test** in `crates/client/src/client.rs`

```rust
#[test]
fn zed_services_disabled_reads_setting() {
    let content: settings::SettingsContent =
        serde_json::from_str(r#"{"disable_zed_services": true}"#).unwrap();
    assert!(ZedServicesSettings::from_settings(&content).disable_zed_services);

    let empty: settings::SettingsContent = serde_json::from_str("{}").unwrap();
    assert!(!ZedServicesSettings::from_settings(&empty).disable_zed_services);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p client zed_services_disabled_reads_setting`
Expected: compile error, `ZedServicesSettings` not found.

- [ ] **Step 3: Implement**

In `crates/settings_content/src/project.rs` after `disable_ai`:

```rust
/// Whether to disable all communication with Zed's servers.
///
/// Default: false
pub disable_zed_services: Option<bool>,
```

In `crates/client/src/client.rs` next to `TelemetrySettings`:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, RegisterSetting)]
pub struct ZedServicesSettings {
    pub disable_zed_services: bool,
}

impl Settings for ZedServicesSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            disable_zed_services: content.project.disable_zed_services.unwrap_or(false),
        }
    }
}
```

Add `"disable_zed_services": false,` to `assets/settings/default.json` beside `"disable_ai"`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p client zed_services_disabled_reads_setting`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/settings_content/src/project.rs crates/client/src/client.rs assets/settings/default.json
git commit -m "client: Add disable_zed_services setting"
```

---

### Task 2: Global flag, host matcher, and startup wiring

**Files:**
- Modify: `crates/http_client/src/http_client.rs` (free functions at the end)
- Modify: `crates/reqwest_client/src/reqwest_client.rs:268` (`send`)
- Modify: `crates/zed/src/main.rs:497` and `:533`
- Test: `crates/http_client/src/http_client.rs` (`mod tests`)

**Interfaces:**
- Consumes: `ZedServicesSettings` (Task 1).
- Produces: `http_client::is_zed_service_host(&str) -> bool`, `http_client::set_zed_services_disabled(bool)`, `http_client::zed_services_disabled() -> bool`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn blocks_zed_owned_hosts_only() {
    for host in ["zed.dev", "api.zed.dev", "cloud.zed.dev", "llm.zed.dev", "staging.zed.dev"] {
        assert!(is_zed_service_host(host), "{host} should be blocked");
    }
    for host in ["localhost", "127.0.0.1", "api.openai.com", "github.com", "notzed.dev"] {
        assert!(!is_zed_service_host(host), "{host} should be allowed");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p http_client blocks_zed_owned_hosts_only`
Expected: compile error.

- [ ] **Step 3: Implement matcher and flag**

```rust
static ZED_SERVICES_DISABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_zed_services_disabled(disabled: bool) {
    ZED_SERVICES_DISABLED.store(disabled, std::sync::atomic::Ordering::Relaxed);
}

pub fn zed_services_disabled() -> bool {
    ZED_SERVICES_DISABLED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn is_zed_service_host(host: &str) -> bool {
    host == "zed.dev" || host.ends_with(".zed.dev")
}
```

- [ ] **Step 4: Add the silent backstop** at the top of `ReqwestClient::send`

```rust
if http_client::zed_services_disabled() {
    if let Some(host) = request.uri().host() {
        if http_client::is_zed_service_host(host) {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| log::debug!("disable_zed_services: dropped request to {host}"));
            return Err(anyhow::anyhow!("disabled by disable_zed_services"));
        }
    }
}
```

The flag is a static, so it applies to both `ReqwestClient` instances in `main.rs` (the one built at `:512` and the one inside `Client::production`, which replaces it at `:528`).

- [ ] **Step 5: Set the flag at startup**

Immediately after `settings::init(cx);` (`crates/zed/src/main.rs:497`):

```rust
http_client::set_zed_services_disabled(ZedServicesSettings::get_global(cx).disable_zed_services);
```

And again inside the existing `cx.observe_global::<SettingsStore>` block at `:533`.

- [ ] **Step 6: Run tests and build**

Run: `cargo test -p http_client blocks_zed_owned_hosts_only && cargo build -p zed --release`
Expected: PASS, build succeeds.

- [ ] **Step 7: Commit**

```bash
git add crates/http_client/src/http_client.rs crates/reqwest_client/src/reqwest_client.rs crates/zed/src/main.rs
git commit -m "http_client: Add Zed host blocklist behind disable_zed_services"
```

---

### Task 3: Telemetry does not start

**Files:**
- Modify: `crates/client/src/telemetry.rs:566` (`report_event`), `:372` (`metrics_enabled`), `:376` (`diagnostics_enabled`)
- Test: `crates/client/src/telemetry.rs`

**Interfaces:**
- Consumes: `http_client::zed_services_disabled()`.
- Produces: no new API. `report_event` early return stops both the queue and its flush timer. `diagnostics_enabled() == false` also stops the minidump upload path in `crates/zed/src/reliability.rs:51`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn disabled_telemetry_queues_nothing(cx: &mut TestAppContext) {
    http_client::set_zed_services_disabled(true);
    let telemetry = cx.update(|cx| Telemetry::new(Arc::new(TestClock), test_http_client(), cx));
    telemetry.report_event(Event::Flexible(telemetry::Event {
        event_type: "Test Event".into(),
        event_properties: Default::default(),
    }));
    assert!(!telemetry.metrics_enabled());
    assert_eq!(telemetry.queued_event_count(), 0);
}
```

Add a `#[cfg(any(test, feature = "test-support"))] fn queued_event_count(&self) -> usize` helper reading `state.events_queue.len()`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p client disabled_telemetry_queues_nothing`
Expected: FAIL, count is 1.

- [ ] **Step 3: Implement**

At the top of `report_event`:

```rust
if http_client::zed_services_disabled() {
    return;
}
```

In `metrics_enabled` and `diagnostics_enabled`, return `false` when disabled.

- [ ] **Step 4: Run tests**

Run: `cargo test -p client telemetry`
Expected: PASS, existing flush tests included.

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/telemetry.rs
git commit -m "client: Skip telemetry when disable_zed_services is set"
```

---

### Task 4: Extension registry does not get called

**Files:**
- Modify: `crates/extension_host/src/extension_host.rs:813` (`fetch_extensions_from_api`), `:458-470` (startup auto-install and update check)
- Modify: `crates/extensions_ui/src/extensions_ui.rs` (empty state)
- Test: `crates/extension_host/src/extension_host.rs`

**Interfaces:**
- Consumes: `http_client::zed_services_disabled()`.
- Produces: no new API.

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
async fn disabled_services_skip_registry_fetch(cx: &mut TestAppContext) {
    http_client::set_zed_services_disabled(true);
    let store = init_test_extension_store(cx).await;
    let extensions = store.update(cx, |store, cx| store.fetch_extensions("", None, cx)).await;
    assert!(extensions.unwrap_or_default().is_empty());
    assert_eq!(test_http_request_count(cx), 0);
}
```

Use the existing test helpers in this file; if there is no request counter, assert on the empty result only and drop the second assertion.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p extension_host disabled_services_skip_registry_fetch`
Expected: FAIL with a request attempted or a connection error.

- [ ] **Step 3: Implement**

At the top of `fetch_extensions_from_api`, return an empty vector when disabled. That one guard covers `fetch_extensions`, `fetch_extensions_with_update_available`, and `fetch_extension`.

In the startup task at `:458-470`, skip `auto_install_extensions` and `check_for_updates` when disabled.

- [ ] **Step 4: Show a reason in the panel**

In `extensions_ui`, when disabled, render "Extension registry disabled by `disable_zed_services`" in place of the list.

- [ ] **Step 5: Run tests**

Run: `cargo test -p extension_host && cargo build -p zed --release`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/extension_host/src/extension_host.rs crates/extensions_ui/src/extensions_ui.rs
git commit -m "extension_host: Skip registry when disable_zed_services is set"
```

---

### Task 5: Close the remaining doors

**Files:**
- Modify: `crates/client/src/client.rs:1042` (`sign_in_with_optional_connect`) and `:1091` (`connect`)
- Modify: `crates/auto_update/src/auto_update.rs:279` (polling guard, for anyone building with a non-dev channel)
- Modify: `crates/edit_prediction_ui/src/edit_prediction_button.rs:355`
- Modify: `crates/edit_prediction/src/edit_prediction.rs` (map `Zed` and `Mercury` to `None`)
- Modify: `crates/web_search_providers/src/web_search_providers.rs:55-62`

**Interfaces:**
- Consumes: `http_client::zed_services_disabled()`.
- Produces: no new API.

- [ ] **Step 1: Refuse sign-in and collab connect**

Return early from `sign_in_with_optional_connect` and from `connect` when disabled, so no future UI affordance can start the flow.

- [ ] **Step 2: Guard the updater regardless of channel**

In `auto_update::init`, treat `disable_zed_services` as forcing `poll_for_updates` false.

- [ ] **Step 3: Hide Zed-hosted providers**

Exclude `Zed` and `Mercury` from the provider list, and dispatch them as `None` so a stale saved value cannot fire a request. Skip registering `CloudWebSearchProvider`.

- [ ] **Step 4: Verify**

Run: `cargo test -p client -p edit_prediction -p web_search_providers`, then start Zed with the flag on and confirm the edit-prediction picker offers only local and BYO providers, and the palette's sign-in action does nothing.

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/client.rs crates/auto_update/src/auto_update.rs crates/edit_prediction_ui/src/edit_prediction_button.rs crates/edit_prediction/src/edit_prediction.rs crates/web_search_providers/src/web_search_providers.rs
git commit -m "editor: Close remaining Zed service doors when disabled"
```

---

### Task 6: End-to-end check, fork, CI, patches

**Files:**
- Create: `.github/workflows/build-patched.yml`
- Create: `patches/` (generated)

**Interfaces:**
- Consumes: the branch from Tasks 1-5.
- Produces: a Linux `zed.tar.gz` artifact and `patches/*.patch`.

- [ ] **Step 1: Prove it on the local build first**

Build and run with the setting on. In `zed: open log`, confirm zero lines containing `zed.dev`, `api.zed.dev`, `cloud.zed.dev`, and zero telemetry flush lines, after a minute of editing and opening the Extensions panel.

- [ ] **Step 2: Fork and branch**

```bash
gh repo fork zed-industries/zed --clone=false --remote=false
git remote add fork git@github.com:abu-matterize/zed.git
git checkout -b patched/no-zed-services
git push -u fork patched/no-zed-services
```

- [ ] **Step 3: Write the workflow**

`.github/workflows/build-patched.yml`, on `workflow_dispatch` and pushes to `patched/**`:

- `runs-on: ubuntu-latest`
- `apt-get install libglib2.0-dev libgtk-3-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-dev libx11-xcb-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libasound2-dev libfontconfig-dev libvulkan-dev libssl-dev`
- Rust toolchain from `rust-toolchain.toml` (1.98.1), target `wasm32-wasip2`
- `Swatinem/rust-cache@v2`
- `cargo build -p zed --release`
- `script/bundle-linux --skip-sentry`
- `actions/upload-artifact@v4` on `dist/*.tar.gz`

- [ ] **Step 4: Run it, fix the dep list**

Expect 2 or 3 failures on missing packages before the build starts. Add only what the compiler or linker asks for.

- [ ] **Step 5: Export patches**

```bash
git format-patch upstream/main -o patches/
git format-patch --check upstream/main
```

Verify in a clean worktree per the project patch workflow, then remove the worktree.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/build-patched.yml patches
git commit -m "ci: Build patched Zed on the fork"
```
