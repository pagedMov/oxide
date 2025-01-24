use crate::{comp::OxideHelper, event::ShError, shellenv::{self, read_meta, read_vars, write_meta, RSH_PGRP}, OxideResult};
use std::path::{Path, PathBuf};
use nix::{sys::signal::{kill, Signal}, unistd::Pid};

use rustyline::{self, config::Configurer, error::ReadlineError, history::{DefaultHistory, History}, ColorMode, Config, EditMode, Editor};

use crate::interp::expand;



fn init_prompt() -> OxideResult<Editor<OxideHelper, DefaultHistory>> {
	let config = build_editor_config()?;
	let mut rl = initialize_editor(config)?;
	load_history(&mut rl)?;
	Ok(rl)
}

fn build_editor_config() -> OxideResult<Config> {
	let mut config = Config::builder();

	let max_size = read_shell_option("max_hist", 1000)?;
	let hist_dupes = read_shell_option_bool("hist_ignore_dupes")?;
	let comp_limit = read_shell_option("comp_limit", 5)?;
	let edit_mode = match read_shell_option("edit_mode", 1)? {
		0 => EditMode::Emacs,
		_ => EditMode::Vi,
	};
	let auto_hist = read_shell_option_bool("auto_hist")?;
	let prompt_highlight = match read_shell_option("prompt_highlight", 1)? {
		0 => ColorMode::Disabled,
		_ => ColorMode::Enabled,
	};
	let tab_stop = read_shell_option("tab_stop", 1).map(|val| val.max(1))?;

	config = config
		.max_history_size(max_size)
		.unwrap_or_else(|e| {
			eprintln!("Invalid max history size: {}", e);
			std::process::exit(1);
		})
	.history_ignore_dups(hist_dupes)
		.unwrap()
		.completion_prompt_limit(comp_limit)
		.edit_mode(edit_mode)
		.auto_add_history(auto_hist)
		.color_mode(prompt_highlight)
		.tab_stop(tab_stop);

		Ok(config.build())
}

fn read_shell_option(option: &str, default: usize) -> OxideResult<usize> {
	read_meta(|m| m.get_shopt(option).unwrap_or(default))
}

fn read_shell_option_bool(option: &str) -> OxideResult<bool> {
	read_meta(|m| m.get_shopt(option).is_some_and(|opt| opt > 0))
}

fn initialize_editor(config: Config) -> OxideResult<Editor<OxideHelper, DefaultHistory>> {
	let mut rl = Editor::with_config(config).unwrap_or_else(|e| {
		eprintln!("Failed to initialize Rustyline editor: {}", e);
		std::process::exit(1);
	});
	rl.set_completion_type(rustyline::CompletionType::List);
	rl.set_helper(Some(OxideHelper::new()));
	Ok(rl)
}

fn load_history(rl: &mut Editor<OxideHelper, DefaultHistory>) -> OxideResult<()> {
	let hist_path = read_vars(|vars| vars.get_evar("HIST_FILE"))?.unwrap_or_else(|| {
		let home = read_vars(|vars| vars.get_evar("HOME").unwrap()).unwrap();
		format!("{}/.oxide_hist", home)
	});
	let hist_path = PathBuf::from(hist_path);
	if let Err(e) = rl.load_history(&hist_path) {
		eprintln!("No previous history found or failed to load history: {}", e);
	}
	Ok(())
}

pub fn run() -> OxideResult<String> {
	write_meta(|m| m.enter_prompt())?;

	let mut rl = init_prompt()?;
	let hist_path = read_vars(|vars| vars.get_evar("HIST_FILE"))?.unwrap_or_else(|| -> String {
		let home = read_vars(|vars| vars.get_evar("HOME").unwrap()).unwrap();
		format!("{}/.oxide_hist",home)
	});
	let prompt = expand::expand_prompt()?;

	match rl.readline(&prompt) {
		Ok(line) => {
			write_meta(|m| m.leave_prompt())?;
			if !line.is_empty() {
				rl.history_mut()
					.add(&line)
					.map_err(|_| ShError::from_internal("Failed to write to history file"))?;
					rl.history_mut()
						.save(Path::new(&hist_path))
						.map_err(|_| ShError::from_internal("Failed to write to history file"))?;
					write_meta(|m| m.set_last_input(&line))?;
			}
			Ok(line)
		}
		Err(ReadlineError::Interrupted) => {
			write_meta(|m| m.leave_prompt())?;
			Ok(String::new())
		}
		Err(ReadlineError::Eof) => {
			write_meta(|m| m.leave_prompt())?;
			kill(Pid::this(), Signal::SIGQUIT).map_err(|_| ShError::from_io())?;
			Ok(String::new())
		}
		Err(e) => {
			write_meta(|m| m.leave_prompt())?;
			Err(ShError::from_internal(format!("rustyline error: {}",e.to_string().as_str()).as_str()))
		}
	}
}