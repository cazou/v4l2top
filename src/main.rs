mod renderers;
mod v4l2_mem;
mod v4l2_stats;

use std::time::Duration;

use anyhow::Result;
use crossterm::event::KeyModifiers;
use ratatui::DefaultTerminal;

#[warn(unused_imports)]
use cli_log::*;

use renderers::TopRenderer;

fn main() -> Result<()> {
    cli_log::init_cli_log!();
    ratatui::run(app)?;
    Ok(())
}

fn app(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut renderer = TopRenderer::new();

    terminal.draw(|frame| renderer.render(frame))?;

    loop {
        if crossterm::event::poll(Duration::from_millis(100))?
            && let Some(event) = crossterm::event::read()?.as_key_press_event()
        {
            match event.code {
                crossterm::event::KeyCode::Char('q') => break Ok(()),
                crossterm::event::KeyCode::Char('c') => {
                    if event.modifiers.contains(KeyModifiers::CONTROL) {
                        break Ok(());
                    }
                }
                crossterm::event::KeyCode::F(2) => {
                    renderer.shift_usage_renderer();
                }
                crossterm::event::KeyCode::F(3) => {
                    renderer.show_bytes_flip();
                }
                crossterm::event::KeyCode::F(4) => {
                    renderer.full_cmd_flip();
                }
                crossterm::event::KeyCode::F(5) => {
                    renderer.codec_only_flip();
                }
                crossterm::event::KeyCode::Up => renderer.select_previous(),
                crossterm::event::KeyCode::Down => renderer.select_next(),
                _ => {}
            }
        }
        terminal.draw(|frame| renderer.render(frame))?;
    }
}
