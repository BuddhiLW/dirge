//! /addons handler: list the Clojure addons, reload them in place, and run
//! the slash commands they register.

use crate::ui::slash::{SlashCtx, c_error};
#[cfg(feature = "addons")]
use crate::ui::slash::{SlashOutcome, c_agent, c_result};
#[cfg(feature = "addons")]
use crate::ui::theme;

pub(crate) async fn cmd_addons(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    #[cfg(not(feature = "addons"))]
    {
        let _ = parts;
        ctx.renderer.write_line(
            "addons are disabled in this build (enable the 'addons' feature)",
            c_error(),
        )?;
        Ok(())
    }

    #[cfg(feature = "addons")]
    match parts.get(1).copied() {
        None | Some("list") => list(ctx),
        Some("reload") => reload(ctx).await,
        Some(other) => {
            ctx.renderer
                .write_line(&format!("unknown /addons subcommand: {other}"), c_error())?;
            ctx.renderer
                .write_line("usage: /addons [list|reload]", c_agent())?;
            Ok(())
        }
    }
}

#[cfg(feature = "addons")]
fn list(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let renderer = &mut *ctx.renderer;
    let Some(host) = crate::addons::global() else {
        renderer.write_line(
            "no addons loaded: put an addon under .dirge/addons/ or ~/.config/dirge/addons/, then /addons reload",
            c_error(),
        )?;
        return Ok(());
    };
    let addons = host.addons();
    renderer.write_line(&format!("loaded {} addon(s):", addons.len()), c_agent())?;
    for addon in &addons {
        let status = addon
            .health
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("?");
        renderer.write_line(&format!("  {} ({status})", addon.id), c_result())?;
        renderer.write_line(
            &format!("    manifest : {}", addon.manifest.display()),
            theme::dim(),
        )?;
        let rows = [
            (
                "tools   ",
                addon
                    .tools
                    .iter()
                    .map(|t| t.exposed_name.clone())
                    .collect::<Vec<_>>(),
            ),
            (
                "hooks   ",
                addon.hooks.iter().map(|h| h.key().to_string()).collect(),
            ),
            (
                "commands",
                addon
                    .commands
                    .iter()
                    .map(|c| format!("/{}", c.name))
                    .collect(),
            ),
        ];
        for (label, names) in rows {
            if !names.is_empty() {
                renderer
                    .write_line(&format!("    {label} : {}", names.join(", ")), theme::dim())?;
            }
        }
    }
    let failures = host.failures();
    if !failures.is_empty() {
        renderer.write_line("failed to load:", c_error())?;
        for failure in &failures {
            renderer.write_line(
                &format!("  {}: {}", failure.manifest.display(), failure.error),
                c_error(),
            )?;
        }
    }
    Ok(())
}

/// Reload every addon, then swap the live agent's addon tools so the next
/// prompt sees them.
#[cfg(feature = "addons")]
async fn reload(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    use std::sync::Arc;

    let settings = ctx.cfg.addons.clone().unwrap_or_default();
    let outcome = tokio::task::spawn_blocking(move || crate::addons::reload(&settings))
        .await
        .map_err(|e| format!("reload task failed: {e}"))
        .and_then(|r| r);
    let (host, report) = match outcome {
        Ok(done) => done,
        Err(error) => {
            ctx.renderer
                .write_line(&format!("addon reload failed: {error}"), c_error())?;
            return Ok(());
        }
    };
    let tools: Vec<Arc<dyn crate::agent::agent_loop::LoopTool>> =
        crate::addons::tool::loop_tools(&host, ctx.permission.clone(), ctx.ask_tx.clone())
            .into_iter()
            .map(|t| Arc::new(t) as Arc<dyn crate::agent::agent_loop::LoopTool>)
            .collect();
    ctx.agent
        .upsert_loop_tools(crate::addons::tool::SOURCE, tools);
    crate::provider::set_current_agent(Arc::new(ctx.agent.clone()));
    #[cfg(feature = "plugin")]
    crate::plugin::tool_bridge::publish_registry(ctx.agent.loop_tools());
    #[cfg(feature = "slash-completion")]
    crate::ui::slash::register_addon_commands(
        host.commands().into_iter().map(|c| c.name).collect(),
    );

    let renderer = &mut *ctx.renderer;
    renderer.write_line(
        &format!(
            "reloaded {} addon(s): {}",
            report.loaded.len(),
            report.loaded.join(", ")
        ),
        c_agent(),
    )?;
    if !report.tools_added.is_empty() {
        renderer.write_line(
            &format!("  + tools: {}", report.tools_added.join(", ")),
            c_result(),
        )?;
    }
    if !report.tools_removed.is_empty() {
        renderer.write_line(
            &format!("  - tools: {}", report.tools_removed.join(", ")),
            c_result(),
        )?;
    }
    for failure in report.failures.iter().chain(&report.source_errors) {
        renderer.write_line(
            &format!("  {}: {}", failure.manifest.display(), failure.error),
            c_error(),
        )?;
    }
    renderer.write_line(
        "  tools and hooks take effect at the next prompt",
        theme::dim(),
    )?;
    Ok(())
}

/// `/name args` for a command an addon registered. An answer with a
/// `prompt` starts a turn on it.
#[cfg(feature = "addons")]
pub(crate) async fn run_command(
    ctx: &mut SlashCtx<'_>,
    host: std::sync::Arc<crate::addons::host::AddonHost>,
    command: crate::addons::domain::CommandSpec,
    text: &str,
) -> anyhow::Result<SlashOutcome> {
    let args = text
        .trim_start()
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest)
        .to_string();
    let name = command.name.clone();
    let outcome = tokio::task::spawn_blocking(move || host.run_command(&command, &args))
        .await
        .map_err(|e| format!("command task failed: {e}"))
        .and_then(|r| r);
    let output = match outcome {
        Ok(output) => output,
        Err(error) => {
            ctx.renderer
                .write_line(&format!("[addon] /{name} failed: {error}"), c_error())?;
            return Ok(SlashOutcome::Handled);
        }
    };
    if let Some(text) = output.text {
        let safe =
            crate::ui::ansi::strip_escapes(&text, crate::ui::ansi::StripPolicy::KEEP_NEWLINE);
        for line in safe.lines() {
            ctx.renderer.write_line(line, c_agent())?;
        }
    }
    Ok(match output.prompt {
        Some(prompt) => SlashOutcome::DeferPromptRun { prompt },
        None => SlashOutcome::Handled,
    })
}
