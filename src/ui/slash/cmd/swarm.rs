//! /swarm handler: open or close the full-screen grid of external panels.

use crate::ui::slash::{SlashCtx, c_agent, c_error};
use crate::ui::swarm::SwarmCmd;

pub(crate) async fn cmd_swarm(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    let cmd = match SwarmCmd::parse(&parts[1..]) {
        Ok(cmd) => cmd,
        Err(usage) => {
            ctx.renderer.write_line(&usage, c_error())?;
            return Ok(());
        }
    };
    let open = match cmd {
        SwarmCmd::Toggle => ctx.renderer.toggle_swarm(),
        SwarmCmd::Open => ctx.renderer.set_swarm_open(true),
        SwarmCmd::Close => ctx.renderer.set_swarm_open(false),
    };
    if !open {
        ctx.renderer.write_line("swarm grid closed", c_agent())?;
    }
    ctx.renderer.render_viewport()?;
    Ok(())
}
