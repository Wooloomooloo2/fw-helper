//! polkit authorization.
//!
//! D-Bus policy is too coarse to express "this user may set a charge limit but not
//! a power limit", so authorization happens here, per action, at call time
//! (ADR 0003). Every method that changes hardware must go through [`check`].

use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use std::time::Duration;

/// Subject type understood by polkit for a D-Bus caller.
const SUBJECT_KIND: &str = "system-bus-name";
const FLAG_NONE: u32 = 0;
/// `AllowUserInteraction` — lets polkit prompt rather than refusing outright.
const FLAG_ALLOW_INTERACTION: u32 = 1;

/// Upper bound on how long we will wait for a human to answer a polkit prompt.
///
/// This is not tuning. **polkit blocks indefinitely when no authentication agent
/// can service the request** — which happens for any caller not attached to a login
/// session, such as a shell with no `XDG_SESSION_ID`. Without a bound, one such call
/// wedges the method handler forever.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// A handle to the tokio runtime, captured while we are still on it.
///
/// **Interface methods do not run on the tokio runtime.** zbus drives them on its own
/// executor (it uses `async-io` by default), so any tokio-specific API called from a
/// handler panics with "there is no reactor running" — measured on 2026-08-22, when the
/// first unprivileged caller reached the interactive polkit path and took the
/// connection's executor thread down with it.
///
/// The bounded prompt below genuinely needs a timer, and the timer needs a runtime, so
/// the work is handed to the runtime that has one rather than assuming we are on it.
static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// Call once from `main`, which *is* on the runtime.
pub fn init_runtime(handle: tokio::runtime::Handle) {
    let _ = RUNTIME.set(handle);
}

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait Authority {
    #[allow(clippy::too_many_arguments)]
    fn check_authorization(
        &self,
        subject: &(&str, HashMap<&str, OwnedValue>),
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

/// Is `sender` allowed to perform `action`?
///
/// Fails **closed**: any error contacting polkit denies the request. A permissive
/// fallback here would mean an unreachable polkit silently granted everyone
/// hardware write access.
pub async fn check(conn: &zbus::Connection, sender: &str, action: &str) -> Result<(), String> {
    let authority = AuthorityProxy::new(conn)
        .await
        .map_err(|e| format!("cannot reach polkit: {e}"))?;

    let mut details = HashMap::new();
    let name = OwnedValue::try_from(zbus::zvariant::Value::from(sender))
        .map_err(|e| format!("cannot encode caller name: {e}"))?;
    details.insert("name", name);
    let subject = (SUBJECT_KIND, details);

    // Ask without interaction first. This always returns promptly, and covers the
    // already-authorized cases: root, an admin whose auth_admin_keep is still
    // valid, or a permissive local rule. Only if that fails do we risk a prompt.
    let (authorized, _challenge, _) = authority
        .check_authorization(&subject, action, HashMap::new(), FLAG_NONE, "")
        .await
        .map_err(|e| format!("polkit check failed: {e}"))?;
    if authorized {
        return Ok(());
    }

    // Authentication is genuinely required. Now allow a prompt, bounded.
    //
    // Run it on the tokio runtime rather than here: this is a zbus executor thread and
    // has no timer, and `tokio::time::timeout` panics without one. The task owns
    // everything it touches, because a spawned task must be 'static.
    let Some(runtime) = RUNTIME.get() else {
        return Err(format!(
            "cannot bound the authentication prompt for {action}: no runtime handle. \
             Refusing rather than waiting forever"
        ));
    };
    let conn = conn.clone();
    let sender = sender.to_string();
    let action_id = action.to_string();

    let bounded = runtime.spawn(async move {
        let authority = AuthorityProxy::new(&conn)
            .await
            .map_err(|e| format!("cannot reach polkit: {e}"))?;
        let mut details = HashMap::new();
        let name = OwnedValue::try_from(zbus::zvariant::Value::from(sender.as_str()))
            .map_err(|e| format!("cannot encode caller name: {e}"))?;
        details.insert("name", name);
        let subject = (SUBJECT_KIND, details);

        let prompt = authority.check_authorization(
            &subject,
            &action_id,
            HashMap::new(),
            FLAG_ALLOW_INTERACTION,
            "",
        );
        match tokio::time::timeout(PROMPT_TIMEOUT, prompt).await {
            Ok(Ok((true, _, _))) => Ok(()),
            Ok(Ok((false, _, _))) => Err(format!("not authorized for {action_id}")),
            Ok(Err(e)) => Err(format!("polkit check failed: {e}")),
            Err(_) => Err(format!(
                "no answer from polkit within {}s for {action_id}. No authentication \
                 agent is available for this caller — run from a desktop session, or \
                 as root",
                PROMPT_TIMEOUT.as_secs()
            )),
        }
    });

    match bounded.await {
        Ok(result) => result,
        // The task itself failed: a panic inside it must surface as a denial, not as a
        // silent success. Fails closed, like every other error on this path.
        Err(e) => Err(format!("polkit check did not complete: {e}")),
    }
}

pub mod actions {
    pub const SET_CHARGE_LIMIT: &str = "org.fwhelper.set-charge-limit";
    pub const SET_FAN: &str = "org.fwhelper.set-fan";
    pub const SET_POWER_LIMIT: &str = "org.fwhelper.set-power-limit";
    /// Start, stop and delete monitoring sessions.
    ///
    /// Still an action rather than an unchecked method, because it makes the daemon
    /// write root-owned files on a caller's say-so — but the policy grants it to any
    /// active session without a prompt. It changes no hardware, and a password dialog
    /// on a Record button is exactly the "trains people to click through prompts"
    /// failure the other actions here are written to avoid.
    pub const RECORD: &str = "org.fwhelper.record";
}
