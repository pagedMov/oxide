//pub mod event;
//pub mod prompt;
//pub mod parser;
//mod rsh;
//
//use log::debug;
//
//use tokio::io::{self, AsyncBufReadExt, BufReader};
//use tokio_stream::{wrappers::LinesStream, StreamExt};
//
//use crate::event::EventLoop;
//
//#[tokio::main]
//async fn main() {
//env_logger::init();
//let mut event_loop = EventLoop::new();
//
//debug!("Starting event loop");
//TODO: use the value returned by this for something
//let _ = event_loop.listen().await;
//}


pub mod prompt;
pub mod event;
pub mod execute;
pub mod shellenv;
pub mod interp;
pub mod builtin;
pub mod comp;
pub mod signal;

use std::{env, fs::OpenOptions, os::fd::AsRawFd, path::PathBuf};

use event::ShError;
use execute::{traverse_ast, OxideWait, RustFd};
use interp::{parse::{descend, NdType}, token::OxideTokenizer};
use nix::{fcntl::OFlag, sys::{stat::Mode, termios::{self, LocalFlags}}, unistd::{Pid,isatty}};
use shellenv::{read_vars, write_meta, write_vars, EnvFlags, RSH_PATH, RSH_PGRP};
use termios::Termios;

//use crate::event::EventLoop;

pub type OxideResult<T> = Result<T, ShError>;

fn set_termios() -> Option<Termios> {
	if isatty(std::io::stdin().as_raw_fd()).unwrap() {
		let mut termios = termios::tcgetattr(std::io::stdin()).unwrap();
		termios.local_flags &= !LocalFlags::ECHOCTL;
		termios::tcsetattr(std::io::stdin(), nix::sys::termios::SetArg::TCSANOW, &termios).unwrap();
		Some(termios)
	} else {
		None
	}
}

fn set_panic_hook() {
	use std::io::Write;
	std::panic::set_hook(Box::new(|info| {
		let crash_log_path = "crash.log"; // Replace with your desired path

		let mut file = match OpenOptions::new()
			.create(true)
			.append(true)
			.open(crash_log_path)
			{
				Ok(file) => file,
				Err(e) => {
					eprintln!("Failed to open crash log file: {}", e);
					return;
				}
			};

		let panic_message = info.payload();

		let location = info.location()
			.map(|loc| format!(" at {}:{}:{}", loc.file(), loc.line(), loc.column()))
			.unwrap_or_else(|| " at unknown location".to_string());

			if let Err(e) = writeln!(file, "Panic occurred: {}{}", panic_message.downcast_ref::<String>().unwrap(), location) {
				eprintln!("Failed to write to crash log: {}", e);
			}
	}));
}

fn restore_termios(orig: &Option<Termios>) {
	if let Some(termios) = orig {
		let fd = std::io::stdin();
		termios::tcsetattr(fd, termios::SetArg::TCSANOW, termios).unwrap();
	}
}

fn initialize_proc_constants() {
	/*
	 * These two variables are set using Lazy,
	 * in order to kick-start the lazy evaluation, we dereference them very early
	 */
	let _ = *RSH_PGRP;
	let _ = *RSH_PATH;
}

#[tokio::main]
async fn main() {
	env_logger::init();
	set_panic_hook();
	initialize_proc_constants();
	let mut interactive = true;
	let mut args = env::args().collect::<Vec<String>>();

	// Ignore SIGTTOU
	signal::sig_handler_setup();

	if args[0].starts_with('-') {
		// TODO: handle unwrap
		let home = read_vars(|vars| vars.get_evar("HOME")).unwrap().unwrap();
		let path = PathBuf::from(format!("{}/.oxide_profile",home));
		if path.exists() {
			shellenv::source_file(path).unwrap();
		}
	}
	if !args.contains(&"--no-rc".into()) && !args.contains(&"--subshell".into()) {
		let home = read_vars(|vars| vars.get_evar("HOME")).unwrap().unwrap();
		let path = PathBuf::from(format!("{}/.oxiderc",home));
		if path.exists() {
			shellenv::source_file(path).unwrap();
		}
	}
	if args.iter().any(|arg| arg == "--subshell") {
		let index = args.iter().position(|arg| arg == "--subshell").unwrap();
		interactive = false;
		args.remove(index);
		write_meta(|m| m.mod_flags(|f| *f |= EnvFlags::IN_SUBSH)).unwrap();
	}
	match interactive {
		true => { // interactive
			let termios = set_termios();
			write_meta(|m| m.mod_flags(|f| *f |= EnvFlags::INTERACTIVE)).unwrap();

			event::main_loop().unwrap();

			restore_termios(&termios);
		},
		false => {
			main_noninteractive(args).unwrap();
		}
	};
}



fn main_noninteractive(args: Vec<String>) -> OxideResult<OxideWait> {
	let mut pos_params: Vec<String> = vec![];
	let input;

	// Input Handling
	if args[1] == "-c" {
		if args.len() < 3 {
			eprintln!("Expected a command after '-c' flag");
			return Ok(OxideWait::Fail { code: 1, cmd: None, });
		}
		input = args[2].clone(); // Store the command string
	} else {
		let script_name = &args[1];
		let path = PathBuf::from(script_name);
		if args.len() > 2 {
			pos_params = args[2..].to_vec();
		}
		let mode = Mode::S_IRUSR | Mode::S_IRGRP;
		let file_desc = RustFd::open(&path, OFlag::O_RDONLY, mode);
		match file_desc {
			Ok(script) => {
				input = script.read().expect("Failed to read from script FD");
			}
			Err(e) => {
				eprintln!("Error opening file: {}\n", e);
				return Ok(OxideWait::Fail { code: 1, cmd: None, });
			}
		}
	}

	// Code Execution Logic
	write_meta(|m| m.set_last_input(&input))?;
	for (index,param) in pos_params.into_iter().enumerate() {
		let key = format!("{}",index + 1);
		write_vars(|v| v.set_param(key, param))?;
	}
	let mut tokenizer = OxideTokenizer::new(&input);
	let mut last_result = OxideWait::new();
	loop {
		let state = descend(&mut tokenizer);
		match state {
			Ok(parse_state) => {
				if deconstruct!(NdType::Root { deck }, &parse_state.ast.nd_type, {
					deck.is_empty()
				}) { break Ok(OxideWait::Success) }
				let result = traverse_ast(parse_state.ast, None);
				match result {
					Ok(code) => {
						if let OxideWait::Fail { code, cmd } = code {
							panic!()
						} else {
							last_result = code;
						}
					}
					Err(e) => {
						event::throw(e).unwrap();
					}
				}
			}
			Err(e) => {
				event::throw(e).unwrap();
			}
		}
	}
}

//#[tokio::main]
//async fn main() {
//env_logger::init();
//loop {
//let mut stdout = io::stdout();
//stdout.write_all(b"> ").await.unwrap();
//stdout.flush().await.unwrap();
//
//let stdin = io::stdin();
//let mut reader = BufReader::new(stdin);
//
//let mut input = String::new();
//reader.read_line(&mut input).await.unwrap();
//
//let trimmed_input = input.trim().to_string();
//trace!("Received input! {}",trimmed_input);
//let shellenv = ShellEnv::new(false,false);
//let state = descend(&input,&shellenv);
//if let Err(e) = state {
//println!("{}",e);
//} else {
//println!("debug tree:");
//println!("{}",state.unwrap().ast);
//}
//}
//}