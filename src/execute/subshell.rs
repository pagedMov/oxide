use crate::expand;
use crate::helper;
use crate::prelude::*;

use crate::shellenv::ChildProc;
use crate::shellenv::JobBuilder;
use crate::utils;

use super::dispatch;

pub fn exec_subshell<'a>(subsh: Pair<'a,Rule>, slash: &mut Slash) -> SlashResult<()> {
	let mut shebang = None;
	let body = subsh.scry(Rule::subsh_body).unpack()?.as_str();
	if let Some(subshebang) = subsh.scry(Rule::subshebang) {
		let raw_shebang = subshebang.as_str().to_string();
		shebang = Some(expand::misc::expand_shebang(slash,&raw_shebang));
	}

	let argv = helper::prepare_argv(subsh.clone(),slash)?;
	let redirs = helper::prepare_redirs(subsh)?;

	slash.ctx_mut().extend_redirs(redirs);
	if let Some(shebang) = shebang {
		let script = format!("{}{}",shebang,body);
		handle_external_subshell(script,argv,slash)?;
	} else {
		handle_internal_subshell(body.to_string(),argv,slash)?;
	}

	slash.set_code(0);
	Ok(())
}

fn handle_external_subshell(script: String, argv: VecDeque<String>, slash: &mut Slash) -> SlashResult<()> {
	let argv = argv.into_iter().map(|arg| CString::new(arg).unwrap()).collect::<Vec<_>>();
	let envp = slash.get_cstring_evars()?;
	let mut memfd = utils::SmartFD::new_memfd("anonymous_subshell", true)?;
	write!(memfd,"{}",script)?;

	let fd_path = CString::new(format!("/proc/self/fd/{memfd}")).unwrap();
	slash.ctx_mut().activate_redirs()?;

	if slash.in_pipe() {
		execve(&fd_path, &argv, &envp).unwrap();
		panic!("execve() failed in subshell execution");
	}

	match unsafe { fork() } {
		Ok(ForkResult::Child) => {
			execve(&fd_path, &argv, &envp).unwrap();
			panic!("execve() failed in subshell execution");
		}
		Ok(ForkResult::Parent { child }) => {
			let children = vec![
				ChildProc::new(child, Some("anonymous_subshell"),None)?
			];
			let job = JobBuilder::new()
				.with_pgid(child)
				.with_children(children)
				.build();
			helper::handle_fg(slash,job)?;
		}
		Err(e) => panic!("Encountered fork error: {}",e)
	}
	memfd.close()?;
	Ok(())
}

fn handle_internal_subshell(body: String, argv: VecDeque<String>, slash: &mut Slash) -> SlashResult<()> {
	let snapshot = slash.clone();
	slash.ctx_mut().activate_redirs()?;
	slash.vars_mut().reset_params();
	for arg in argv {
		slash.vars_mut().pos_param_pushback(&arg);
	}
	dispatch::exec_input(body.consume_escapes(), slash)?;
	*slash = snapshot;
	Ok(())
}
