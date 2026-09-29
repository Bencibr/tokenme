//! `tokenme` — cross-tool AI token usage and cost from the tools' own logs.
//!
//! This is the Linux deliverable and the debugging tool for every other surface:
//! it drives the same incremental index and the same `usage_core::report`
//! aggregation the menu-bar app uses.

mod args;
mod commands;
mod context;
mod render;

use clap::{CommandFactory, Parser};

use args::{Cli, Cmd, PricingCmd};

fn main() {
    // Adapters are independent code that may panic while under construction; a
    // captured panic becomes a per-tool error line instead of a dead process.
    context::install_panic_capture();

    let cli = Cli::parse();

    // Caps, price provenance and icons live outside the index, so these answer
    // without building either.
    if let Some(Cmd::Budget { ref action }) = cli.command {
        if let Err(msg) = commands::budget(action, cli.g.json) {
            fail(&msg);
        }
        return;
    }
    if matches!(cli.command, Some(Cmd::Icons)) {
        if let Err(msg) = commands::icons(cli.g.json) {
            fail(&msg);
        }
        return;
    }

    // Same for a price: the answer is in the price table, and building an index
    // to look one model up would be absurd.
    if let Some(Cmd::Pricing { action: PricingCmd::Explain { ref models } }) = cli.command {
        if models.is_empty() {
            fail("name at least one model, e.g. `tokenme pricing explain deepseek-v4-flash`");
        }
        let opts = match context::pricing_options(&cli.g) {
            Ok(opts) => opts,
            Err(context::Fail(msg)) => fail(&msg),
        };
        let map = usage_core::PricingMap::load(&opts);
        if let Err(msg) = commands::pricing_explain(&map, models, cli.g.json, render::colour_wanted()) {
            fail(&msg);
        }
        return;
    }

    let Some(command) = cli.command else {
        // No args is a question, not an error: answer it and get out.
        let _ = Cli::command().print_help();
        println!();
        return;
    };

    let mut ctx = match context::build(&cli.g) {
        Ok(ctx) => ctx,
        Err(context::Fail(msg)) => fail(&msg),
    };

    // Every reporting command refreshes the index first; `detect`, `index` and
    // `icons` drive it explicitly or not at all.
    if !matches!(command, Cmd::Detect | Cmd::Index { .. } | Cmd::Icons) {
        if let Err(msg) = ctx.prepare() {
            fail(&msg);
        }
    }

    let result: Result<(), String> = match command {
        Cmd::Detect => commands::detect(&ctx),
        Cmd::Daily { days } => commands::periods(&ctx, commands::Grain::Day, days),
        Cmd::Weekly { weeks } => commands::periods(&ctx, commands::Grain::Week, weeks),
        Cmd::Monthly { months } => commands::periods(&ctx, commands::Grain::Month, months),
        Cmd::Report { window, group } => commands::report(&ctx, window, group),
        Cmd::Sessions { limit } => commands::sessions(&ctx, limit),
        Cmd::Quota => commands::quota(&ctx),
        Cmd::DshDoctor => commands::dsh_doctor(&ctx),
        Cmd::WorkbuddyLogin => commands::workbuddy_login(),
        // Handled before the index was built; here only so the match stays total.
        Cmd::Budget { .. } | Cmd::Icons | Cmd::Pricing { action: PricingCmd::Explain { .. } } => Ok(()),
        Cmd::Pricing { action: PricingCmd::Contested { limit } } => {
            commands::pricing_contested(&ctx, limit)
        }
        Cmd::Index { rebuild, prune, status, force } => {
            commands::index(&mut ctx, rebuild, prune, status, force)
        }
    };

    if let Err(msg) = result {
        fail(&msg);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("tokenme: {msg}");
    for panic in context::take_panics() {
        eprintln!("tokenme: source panicked — {panic}");
    }
    std::process::exit(1)
}
