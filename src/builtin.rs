use std::{collections::VecDeque, env, ffi::CString, fs, os::{fd::AsRawFd, unix::fs::{FileTypeExt, MetadataExt}}, path::{Path, PathBuf}};

use nix::unistd::{access, fork, getegid, geteuid, isatty, setpgid, AccessFlags, ForkResult};
use pest::iterators::Pair;

use crate::{error::{LashErr::*, LashErrHigh, LashErrLow}, execute::{CmdRedirs, ExecCtx, ExecFlags, Redir, RustFd}, helper::{self, StrExtension}, shellenv::{self, read_logic, read_vars, write_jobs, write_vars, ChildProc, JobBuilder}, LashResult, OptPairExt, PairExt, Rule};

pub const BUILTINS: [&str; 41] = [
	"return", "break", "contine", "exit", "command", "pushd", "popd", "setopt", "getopt", "type", "string", "int", "bool", "arr", "float", "dict", "expr", "echo", "jobs", "unset", "fg", "bg", "set", "builtin", "test", "[", "shift", "unalias", "alias", "export", "cd", "readonly", "declare", "local", "unset", "trap", "node", "exec", "source", "read_func", "wait",
];

bitflags::bitflags! {
	#[derive(Debug)]
	pub struct EchoFlags: u8 {
		const USE_ESCAPE = 0b00001;
		const NO_NEWLINE = 0b00010;
		const NO_ESCAPE = 0b00100;
		const STDERR = 0b01000;
		const EXPAND_OX_ESC = 0b10000;
	}
	pub struct CdFlags: u8 {
		const CHANGE = 0b0001;
		const PUSH = 0b0010;
		const POP = 0b0100;
	}
}

pub fn catstr(mut c_strings: VecDeque<CString>,newline: bool) -> CString {
	let mut cat: Vec<u8> = vec![];
	let newline_bytes = b"\n\0";
	let space_bytes = b" ";

	while let Some(c_string) = c_strings.pop_front() {
		let bytes = c_string.to_bytes_with_nul();
		if c_strings.is_empty() {
			// Last string: include the null terminator
			cat.extend_from_slice(&bytes[..bytes.len() - 1]);
		} else {
			// Intermediate strings: exclude the null terminator and add whitespace
			cat.extend_from_slice(&bytes[..bytes.len() - 1]);
			cat.extend_from_slice(space_bytes);
		}
	}
	if newline {
		cat.extend_from_slice(newline_bytes);
	} else {
		cat.extend_from_slice(b"\0");
	}

	CString::from_vec_with_nul(cat).unwrap()
}

pub fn run_test<T,F1,F2>(arg: Option<Pair<Rule>>,alter: F1,check_property: F2) -> LashResult<bool>
where F1: FnOnce(&str) -> LashResult<T>, F2: FnOnce(&T) -> bool {
	if arg.is_none() {
		return Err(Low(LashErrLow::ExecFailed("Missing operand in this test call".into())))
	}
	let altered = alter(arg.unwrap().as_str()).map_err(|_| false);
	if altered.is_err() {
		return Ok(false)
	}
	Ok(check_property(&altered.unwrap()))
}

pub fn do_cmp<T,F1,F2>(lhs: &str, rhs: Option<Pair<Rule>>, alter: F1, cmp: F2) -> LashResult<bool>
where F1: Fn(&str) -> LashResult<T>, F2: FnOnce(&T,&T) -> bool {
	if rhs.is_none() {
		return Err(Low(LashErrLow::ExecFailed("Missing operand in this test call".into())))
	}
	let lhs = alter(lhs).map_err(|_| false);
	let rhs = alter(rhs.unwrap().as_str()).map_err(|_| false);
	if lhs.is_err() || rhs.is_err() {
		return Ok(false)
	}
	Ok(cmp(&lhs.unwrap(),&rhs.unwrap()))
}

fn do_log_op<'a>(args: &mut Vec<Pair<'a,Rule>>, result: bool, operator: &str, ctx: &mut ExecCtx) -> LashResult<bool> {
	let rec_result = test(args,ctx)?;
	match operator {
		"!" => Ok(!rec_result),
		"-a" => Ok(result && rec_result),
		"-o" => Ok(result || rec_result),
		_ => unreachable!()
	}
}

/// The test function is a special snowflake and takes a mutable reference to an already prepared arg vector
/// instead of a raw pair like the other builtins. This is to make recursion with -a/-o flags easier
pub fn test<'a>(test_call: &mut Vec<Pair<Rule>>, ctx: &mut ExecCtx) -> LashResult<bool> {
	let blame = test_call.clone();
	if test_call.first().is_some_and(|arg| arg.as_str() == "]") {
		*test_call = Vec::from(&test_call[1..]); // Ignore it
	}
	// Here we define some useful closures to use later
	let is_int = |arg: &str| -> bool { arg.parse::<i32>().is_ok() };
	let to_int = |arg: &str| -> LashResult<i32> {
		arg.parse::<i32>().map_err(|_| Low(LashErrLow::InvalidSyntax("Expected an integer for this test flag".into())))
	};
	let is_path = |arg: &str| -> bool { Path::new(arg).exists() };
	let to_meta = |arg: &str| -> LashResult<fs::Metadata> {
		fs::metadata(arg).map_err(|_| Low(LashErrLow::InvalidSyntax("Invalid path used in test".into())))
	};
	let str_no_op = |arg: &str| -> LashResult<String> { Ok(arg.to_string()) };
	let mut result = false;

	// Now we will use our helper functions and pass those closures to use for type conversions on the arg string
	if let Some(arg) = test_call.pop() {
		result = match arg.as_str() {
			"!" => do_log_op(test_call, true, arg.as_str(), ctx)?,
			"-t" => run_test(test_call.pop(), to_int, |int| isatty(*int).is_ok())?,
			"-b" => run_test(test_call.pop(), to_meta, |meta| meta.file_type().is_block_device())?,
			"-c" => run_test(test_call.pop(), to_meta, |meta| meta.file_type().is_char_device())?,
			"-d" => run_test(test_call.pop(), to_meta, |meta| meta.is_dir())?,
			"-f" => run_test(test_call.pop(), to_meta, |meta| meta.is_file())?,
			"-g" => run_test(test_call.pop(), to_meta, |meta| meta.mode() & 0o2000 != 0)?, // check setgid bit
			"-G" => run_test(test_call.pop(), to_meta, |meta| meta.gid() == u32::from(getegid()))?,
			"-h" => run_test(test_call.pop(), to_meta, |meta| meta.is_symlink())?,
			"-L" => run_test(test_call.pop(), to_meta, |meta| meta.is_symlink())?,
			"-k" => run_test(test_call.pop(), to_meta, |meta| meta.mode() & 0o1000 != 0)?, // check sticky bit
			"-N" => run_test(test_call.pop(), to_meta, |meta| meta.mtime() > meta.atime())?,
			"-O" => run_test(test_call.pop(), to_meta, |meta| meta.uid() == u32::from(geteuid()))?,
			"-p" => run_test(test_call.pop(), to_meta, |meta| meta.file_type().is_fifo())?,
			"-s" => run_test(test_call.pop(), to_meta, |meta| meta.len() > 0)?,
			"-S" => run_test(test_call.pop(), to_meta, |meta| meta.file_type().is_socket())?,
			"-u" => run_test(test_call.pop(), to_meta, |meta| meta.mode() & 0o4000 != 0)?, // check setuid bit
			"-n" => run_test(test_call.pop(), str_no_op, |st| !st.is_empty())?, // check setuid bit
			"-z" => run_test(test_call.pop(), str_no_op, |st| st.is_empty())?,
			"-e" => run_test(test_call.pop(), str_no_op, |st| Path::new(st).exists())?,
			"-r" => run_test(test_call.pop(), str_no_op, |st| access(Path::new(st),AccessFlags::R_OK).is_ok())?,
			"-w" => run_test(test_call.pop(), str_no_op, |st| access(Path::new(st),AccessFlags::W_OK).is_ok())?,
			"-x" => run_test(test_call.pop(), str_no_op, |st| access(Path::new(st),AccessFlags::X_OK).is_ok())?,
			_ if is_int(&arg.as_str()) => {
				if let Some(cmp) = test_call.pop() {
					match cmp.as_str() {
						"-eq" => do_cmp(arg.as_str(),test_call.pop(), to_int, |lhs, rhs| lhs == rhs)?,
						"-ge" => do_cmp(arg.as_str(),test_call.pop(), to_int, |lhs, rhs| lhs >= rhs)?,
						"-gt" => do_cmp(arg.as_str(),test_call.pop(), to_int, |lhs, rhs| lhs > rhs)?,
						"-le" => do_cmp(arg.as_str(),test_call.pop(), to_int, |lhs, rhs| lhs <= rhs)?,
						"-lt" => do_cmp(arg.as_str(),test_call.pop(), to_int, |lhs, rhs| lhs < rhs)?,
						"-ne" => do_cmp(arg.as_str(),test_call.pop(), to_int, |lhs, rhs| lhs != rhs)?,
						_ => return Err(Low(LashErrLow::InvalidSyntax("Expected an integer after comparison flag in test call".into())))
					}
				} else {
					return Err(Low(LashErrLow::InvalidSyntax("Expected a comparison flag after integer in test call".into())))
				}
			}
			_ if is_path(arg.as_str()) && test_call.last().is_some_and(|arg| matches!(arg.as_str(), "-ef" | "nt" | "-ot")) => {
				let cmp = test_call.pop().unwrap();
				match cmp.as_str() {
					"-ef" => do_cmp(cmp.as_str(), test_call.pop(), to_meta, |lhs, rhs| lhs.dev() == rhs.dev())?,
					"-nt" => do_cmp(cmp.as_str(), test_call.pop(), to_meta, |lhs, rhs| lhs.mtime() > rhs.mtime())?,
					"-ot" => do_cmp(cmp.as_str(), test_call.pop(), to_meta, |lhs, rhs| lhs.mtime() < rhs.mtime())?,
					_ => unreachable!()
				}
			}
			_ => {
				if test_call.is_empty() {
					!arg.as_str().is_empty()
				} else if matches!(arg.as_str(), "=") {
					// First arg encountered is an equal sign for some reason. Most likely, an expansion returned nothing, and now there's no word here.
					// Therefore, the only situations which return true are an equally empty right hand side, or a logical continuation flag
					let result = test_call.is_empty() || test_call.last().is_some_and(|arg| matches!(arg.as_str(), "-o" | "-a"));

					if test_call.last().is_some_and(|arg| !matches!(arg.as_str(), "-o" | "-a")) {
						test_call.pop();
					}
					result
				} else if let Some(cmp) = test_call.pop() {
					match cmp.as_str() {
						"=" => do_cmp(arg.as_str(), test_call.pop(), str_no_op, |lhs, rhs| lhs == rhs)?,
						"!=" => do_cmp(arg.as_str(), test_call.pop(), str_no_op, |lhs, rhs| lhs != rhs)?,
						_ => {
							if cmp.as_str() == "==" {
								return Err(Low(LashErrLow::InvalidSyntax("'==' is not a valid comparison operator for test calls. Use '=' instead.".into())));
							} else {
								return Err(Low(LashErrLow::InvalidSyntax("Expected either '==' or '!=' after string in test call".into())));
							}
						}
					}
				} else {
					return Err(Low(LashErrLow::InvalidSyntax("Expected either '==' or '!=' after string in test call".into())));
				}
			}
		};
		if let Some(arg) = test_call.pop() {
			let word = arg.as_str();
			if word == "-a" && !result {
				return Ok(result); // Short-circuit AND if already false
			}
			if word == "-o" && result {
				return Ok(result); // Short-circuit OR if already true
			}
			if word == "-a" || word == "-o" {
				result = do_log_op(test_call, result, word, ctx)?;
			} else {
				return Err(Low(LashErrLow::InvalidSyntax("Unexpected extra argument found in test call".into())));
			}
		}
	}
	Ok(result)
}

pub fn cd<'a>(cd_call: Pair<'a,Rule>, ctx: &mut ExecCtx) -> LashResult<()> {
	let blame = cd_call.clone();
	let mut inner = cd_call.into_inner();
	inner.next(); // Ignore 'cd'
	let arg = inner.next();
	let new_pwd;
	match arg {
		Some(arg) => {
			if arg.as_str() == "-" {
				new_pwd = read_vars(|vars| vars.get_evar("OLDPWD").unwrap_or("/".into()))?;
			} else {
				new_pwd = arg.as_str().into();
			}
		}
		None => {
			new_pwd = env::var("HOME").unwrap_or("/".into());
		}
	}
	env::set_current_dir(new_pwd).map_err(|_| High(LashErrHigh::io_err(blame)))?;
	write_vars(|v| v.export_var("PWD", env::current_dir().unwrap().to_str().unwrap()))?;
	Ok(())
}

pub fn alias<'a>(alias_call: Pair<'a,Rule>, ctx: &mut ExecCtx) -> LashResult<()> {
	let mut inner1 = alias_call.clone().into_inner();
	let mut inner2 = alias_call.into_inner(); // Need two, one for redir processing, one for arg processing
	inner1.next(); // Ignore 'alias'
	inner2.next();
	let mut stdout = RustFd::new(1)?;

	while let Some(arg) = inner2.next() {
		match arg.as_rule() {
			Rule::redir => ctx.push_redir(Redir::from_pair(arg)?),
			_ => { /* Do nothing */}
		}
	}

	let ctx_redirs = ctx.redirs();
	if !ctx_redirs.is_empty() {
		let mut redirs = CmdRedirs::new(ctx_redirs);
		redirs.activate()?;
	}

	while let Some(arg) = inner1.next() {
		match arg.as_rule() {
			Rule::arg_assign => {
				let mut assign_inner = arg.into_inner();
				let alias = assign_inner.next().unpack()?.as_str();
				let body = assign_inner.next().map(|pair| pair.as_str()).unwrap_or_default();
				helper::write_alias(alias, &body.trim_quotes())?;
			}
			Rule::word => {
				let alias = read_logic(|l| l.get_alias(arg.as_str()))?;
				if let Some(alias) = alias {
					let fmt_output = format!("{alias}\n");
					stdout.write(fmt_output.as_bytes())?;
				}
			}
			_ => unreachable!()
		}
	}
	Ok(())
}

pub fn source<'a>(src_call: Pair<'a,Rule>, ctx: &mut ExecCtx) -> LashResult<()> {
	let mut inner = src_call.into_inner();
	inner.next(); // Ignore 'source'
	while let Some(arg) = inner.next() {
		if arg.as_rule() == Rule::word {
			let blame = arg.clone();
			let path = PathBuf::from(arg.as_str());
			if path.exists() && path.is_file() {
				shellenv::source_file(path)?;
			} else {
				let msg = String::from("source failed: File not found");
				return Err(High(LashErrHigh::exec_err(msg, blame)))
			}
		}
	}
	Ok(())
}

pub fn pwd<'a>(pwd_call: Pair<'a,Rule>, ctx: &mut ExecCtx) -> LashResult<()> {
	let blame = pwd_call.clone();
	let mut inner = pwd_call.into_inner();

	while let Some(arg) = inner.next() {
		if arg.as_rule() == Rule::redir {
			ctx.push_redir(Redir::from_pair(arg)?);
		}
	}

	let redirs = ctx.redirs();
	if !redirs.is_empty() {
		let mut redirs = CmdRedirs::new(redirs);
		redirs.activate()?;
	}

	if let Ok(pwd) = env::var("PWD") {
		let stdout = RustFd::new(1)?;
		stdout.write(pwd.as_bytes())?;
		Ok(())
	} else {
		let msg = String::from("PWD environment variable is unset");
		Err(High(LashErrHigh::exec_err(msg, blame)))
	}
}

pub fn export<'a>(export_call: Pair<'a,Rule>, ctx: &mut ExecCtx) -> LashResult<()> {
	let blame = export_call.clone();
	let mut inner = export_call.into_inner();
	while let Some(arg) = inner.next() {
		match arg.as_rule() {
			Rule::cmd_name => continue,
			Rule::arg_assign => {
				let mut assign_inner = arg.into_inner();
				let var_name = assign_inner.next().unpack()?.as_str();
				let val = assign_inner.next().map(|pair| pair.as_str()).unwrap_or_default();
				write_vars(|v| v.export_var(var_name, val))?;
			}
			_ => {
				let msg = String::from("Expected an assignment in export args, got this");
				return Err(High(LashErrHigh::syntax_err(msg, blame)))
			}
		}
	}

	Ok(())
}

pub fn echo<'a>(echo_call: Pair<'a,Rule>, ctx: &mut ExecCtx) -> LashResult<()> {
	let mut flags = EchoFlags::empty();
	let blame = echo_call.clone();
	let mut inner = echo_call.into_inner();
	let mut argv = vec![];

	while let Some(arg) = inner.next() {
		match arg.as_rule() {
			Rule::cmd_name => continue,
			Rule::word => {
				if arg.as_str().starts_with('-') {
					let mut options = arg.as_str().strip_prefix('-').unwrap().chars();
					while let Some(opt) = options.next() {
						match opt {
							'e' => {
								if flags.contains(EchoFlags::NO_ESCAPE) {
									flags &= !EchoFlags::NO_ESCAPE
								}
								flags |= EchoFlags::USE_ESCAPE
							}
							'r' => flags |= EchoFlags::STDERR,
							'n' => flags |= EchoFlags::NO_NEWLINE,
							'P' => flags |= EchoFlags::EXPAND_OX_ESC,
							'E' => {
								if flags.contains(EchoFlags::USE_ESCAPE) {
									flags &= !EchoFlags::USE_ESCAPE
								}
								flags |= EchoFlags::NO_ESCAPE
							}
							_ => break
						}
						if flags.is_empty() {
							argv.push(CString::new(arg.as_str().trim_quotes()).unwrap());
						}
					}
				} else {
					argv.push(CString::new(arg.as_str().trim_quotes()).unwrap());
				}
			}
			Rule::redir => ctx.push_redir(Redir::from_pair(arg)?),
			_ => unreachable!()
		}
	}

	let newline = !flags.contains(EchoFlags::NO_NEWLINE);

	let output = catstr(VecDeque::from(argv), newline);
	let io = ctx.io_mut();
	let target_fd = if flags.contains(EchoFlags::STDERR) {
		if let Some(ref err_fd) = io.stderr {
			err_fd.lock().unwrap().dup().unwrap_or_else(|_| RustFd::from_stderr().unwrap())
		} else {
			RustFd::new(2)?
		}
	} else if let Some(ref out_fd) = io.stdout {
			out_fd.lock().unwrap().dup().unwrap_or_else(|_| RustFd::from_stdout().unwrap())
		} else {
			RustFd::new(1)?
	};

	if let Some(ref fd) = io.stderr {
		if !flags.contains(EchoFlags::STDERR) {
			let fd = fd.lock().unwrap();
			target_fd.dup2(&fd.as_raw_fd())?;
		}
	}

	let mut redirs = CmdRedirs::new(ctx.redirs());
	redirs.activate()?;

	if ctx.flags().contains(ExecFlags::NO_FORK) {
		target_fd.write(output.as_bytes())?;
	}
	match unsafe { fork() } {
		Ok(ForkResult::Child) => {
			target_fd.write(output.as_bytes()).unwrap();
			std::process::exit(0);
		}
		Ok(ForkResult::Parent { child }) => {
			redirs.close_all()?;

			setpgid(child, child).map_err(|_| High(LashErrHigh::io_err(blame.clone())))?;
			let children = vec![
				ChildProc::new(child, Some("echo"), None)?
			];
			let job = JobBuilder::new()
				.with_pgid(child)
				.with_children(children)
				.build();

			if ctx.flags().contains(ExecFlags::BACKGROUND) {
				write_jobs(|j| j.insert_job(job,false))??;
			} else {
				helper::handle_fg(job);
			}
		}
		Err(_) => return Err(High(LashErrHigh::exec_err("Failed to fork in echo()", blame)))
	}

	Ok(())
}
