use std::sync::Arc;

use ahash::AHashMap;
use mail_parser::Message;

use crate::{
    compiler::grammar::{instruction::Instruction, Capability},
    Context, Envelope, Event, Input, Runtime, Sieve, MAX_LOCAL_VARIABLES, MAX_MATCH_VARIABLES,
};

use super::{
    actions::action_include::IncludeResult,
    tests::{mime::NestedParts, test_envelope::parse_envelope_address, TestResult},
    RuntimeError,
};

#[derive(Clone)]
pub(crate) struct ScriptStack {
    pub(crate) script: Arc<Sieve>,
    pub(crate) prev_pos: usize,
    pub(crate) prev_vars_local: Vec<String>,
    pub(crate) prev_vars_match: Vec<String>,
}

impl<'x> Context<'x> {
    pub(crate) fn new(runtime: &'x Runtime, raw_message: &'x [u8]) -> Self {
        Context {
            #[cfg(test)]
            runtime: runtime.clone(),
            #[cfg(not(test))]
            runtime,
            message: Message::parse(raw_message).unwrap_or_default(),
            part: 0,
            part_iter: Vec::new().into_iter(),
            part_iter_stack: Vec::new(),
            pos: usize::MAX,
            test_result: false,
            script_cache: AHashMap::new(),
            script_stack: Vec::with_capacity(0),
            vars_global: AHashMap::new(),
            vars_local: Vec::with_capacity(0),
            vars_match: Vec::with_capacity(0),
            envelope: Vec::new(),
            header_insertions: Vec::new(),
            header_deletions: Vec::new(),
            message_size: usize::MAX,
            part_replacements: Vec::new(),
            part_deletions: Vec::new(),
        }
    }

    #[allow(clippy::while_let_on_iterator)]
    pub fn run(&mut self, input: Input) -> Option<Result<Event, RuntimeError>> {
        let _message = Message::default();
        let message = &_message;

        match input {
            Input::True => self.test_result ^= true,
            Input::False => self.test_result ^= false,
            Input::Script { name, script } => {
                let num_vars = script.num_vars;
                let num_match_vars = script.num_match_vars;

                if num_match_vars > MAX_MATCH_VARIABLES || num_vars > MAX_LOCAL_VARIABLES {
                    return Some(Err(RuntimeError::IllegalAction));
                }

                if self.message_size == usize::MAX {
                    self.message_size = message.raw_message.len();
                }

                self.script_cache.insert(name, script.clone());
                self.script_stack.push(ScriptStack {
                    script,
                    prev_pos: self.pos,
                    prev_vars_local: std::mem::replace(
                        &mut self.vars_local,
                        vec![String::with_capacity(0); num_vars],
                    ),
                    prev_vars_match: std::mem::replace(
                        &mut self.vars_match,
                        vec![String::with_capacity(0); num_match_vars],
                    ),
                });
                self.pos = 0;
                self.test_result = false;
            }
        }

        let mut current_script = self.script_stack.last()?.script.clone();
        let mut iter = current_script.instructions.get(self.pos..)?.iter();

        while let Some(instruction) = iter.next() {
            //println!("{:?}", instruction);
            match instruction {
                Instruction::Jz(jmp_pos) => {
                    if !self.test_result {
                        debug_assert!(*jmp_pos > self.pos);
                        self.pos = *jmp_pos;
                        iter = current_script.instructions.get(self.pos..)?.iter();
                        continue;
                    }
                }
                Instruction::Jnz(jmp_pos) => {
                    if self.test_result {
                        debug_assert!(*jmp_pos > self.pos);
                        self.pos = *jmp_pos;
                        iter = current_script.instructions.get(self.pos..)?.iter();
                        continue;
                    }
                }
                Instruction::Jmp(jmp_pos) => {
                    debug_assert_ne!(*jmp_pos, self.pos);
                    self.pos = *jmp_pos;
                    iter = current_script.instructions.get(self.pos..)?.iter();
                    continue;
                }
                Instruction::Test(test) => match test.exec(self, message) {
                    TestResult::Bool(result) => {
                        self.test_result = result;
                    }
                    TestResult::Event { event, is_not } => {
                        self.pos += 1;
                        self.test_result = is_not;
                        return Some(Ok(event));
                    }
                    TestResult::Error(err) => {
                        return Some(Err(err));
                    }
                },
                Instruction::Clear(clear) => {
                    if clear.local_vars_num > 0 {
                        if let Some(local_vars) = self.vars_local.get_mut(
                            clear.local_vars_idx as usize
                                ..(clear.local_vars_idx + clear.local_vars_num) as usize,
                        ) {
                            for local_var in local_vars.iter_mut() {
                                if !local_var.is_empty() {
                                    *local_var = String::with_capacity(0);
                                }
                            }
                        } else {
                            debug_assert!(false, "Failed to clear local variables: {:?}", clear);
                        }
                    }
                    if clear.match_vars != 0 {
                        self.clear_match_variables(clear.match_vars);
                    }
                }
                Instruction::Keep(_) => {
                    println!("Test passed!");
                }
                Instruction::FileInto(fi) => {
                    self.pos += 1;
                    return Some(Ok(Event::FileInto {
                        folder: self.eval_string(&fi.folder).into_owned(),
                        flags: fi
                            .flags
                            .iter()
                            .map(|f| self.eval_string(f).into_owned())
                            .collect(),
                        mailbox_id: fi
                            .mailbox_id
                            .as_ref()
                            .map(|mi| self.eval_string(mi).into_owned()),
                        special_use: fi
                            .special_use
                            .as_ref()
                            .map(|su| self.eval_string(su).into_owned()),
                        copy: fi.copy,
                        create: fi.create,
                    }));
                }
                Instruction::Redirect(r) => {
                    self.pos += 1;
                    return Some(Ok(Event::Redirect {
                        address: self.eval_string(&r.address).into_owned(),
                        copy: r.copy,
                    }));
                }
                Instruction::Discard => (),
                Instruction::Stop => (),
                Instruction::Reject(_) => (),
                Instruction::ForEveryPart(fep) => {
                    if let Some(next_part) = self.part_iter.next() {
                        self.part = next_part;
                    } else if let Some((prev_part, prev_part_iter)) = self.part_iter_stack.pop() {
                        debug_assert!(fep.jz_pos > self.pos);
                        self.part_iter = prev_part_iter;
                        self.part = prev_part;
                        self.pos = fep.jz_pos;
                        iter = current_script.instructions.get(self.pos..)?.iter();
                        continue;
                    } else {
                        self.part = 0;
                        #[cfg(test)]
                        panic!("ForEveryPart executed without items on stack.");
                    }
                }
                Instruction::ForEveryPartPush => {
                    let part_iter = message
                        .find_nested_parts_ids(self, self.part_iter_stack.is_empty())
                        .into_iter();
                    self.part_iter_stack
                        .push((self.part, std::mem::replace(&mut self.part_iter, part_iter)));
                }
                Instruction::ForEveryPartPop(num_pops) => {
                    debug_assert!(
                        *num_pops > 0 && *num_pops <= self.part_iter_stack.len(),
                        "Pop out of range: {} with {} items.",
                        num_pops,
                        self.part_iter_stack.len()
                    );
                    for _ in 0..*num_pops {
                        if let Some((prev_part, prev_part_iter)) = self.part_iter_stack.pop() {
                            self.part_iter = prev_part_iter;
                            self.part = prev_part;
                        } else {
                            break;
                        }
                    }
                }
                Instruction::Replace(_) => (),
                Instruction::Enclose(_) => (),
                Instruction::ExtractText(extract) => extract.exec(self, message),
                Instruction::AddHeader(add_header) => add_header.exec(self),
                Instruction::DeleteHeader(delete_header) => delete_header.exec(self, message),
                Instruction::Set(set) => set.exec(self),
                Instruction::Notify(_) => (),
                Instruction::Vacation(_) => (),
                Instruction::SetFlag(_) => (),
                Instruction::AddFlag(_) => (),
                Instruction::RemoveFlag(_) => (),
                Instruction::Include(include) => match include.exec(self) {
                    IncludeResult::Cached(script) => {
                        self.script_stack.push(ScriptStack {
                            script: script.clone(),
                            prev_pos: self.pos + 1,
                            prev_vars_local: std::mem::replace(
                                &mut self.vars_local,
                                vec![String::with_capacity(0); script.num_vars],
                            ),
                            prev_vars_match: std::mem::replace(
                                &mut self.vars_match,
                                vec![String::with_capacity(0); script.num_match_vars],
                            ),
                        });
                        self.pos = 0;
                        current_script = script;
                        iter = current_script.instructions.iter();
                        continue;
                    }
                    IncludeResult::Event(event) => {
                        self.pos += 1;
                        return Some(Ok(event));
                    }
                    IncludeResult::Error(err) => {
                        return Some(Err(err));
                    }
                    IncludeResult::None => (),
                },
                Instruction::Convert(_) => (), //TODO
                Instruction::Return => {
                    if let Some(prev_script) = self.script_stack.pop() {
                        self.pos = prev_script.prev_pos;
                        self.vars_local = prev_script.prev_vars_local;
                        self.vars_match = prev_script.prev_vars_match;
                    }
                    current_script = self.script_stack.last()?.script.clone();
                    iter = current_script.instructions.get(self.pos..)?.iter();
                    continue;
                }
                Instruction::Require(capabilities) => {
                    for capability in capabilities {
                        if !self.runtime.allowed_capabilities.contains(capability) {
                            return Some(Err(
                                if let Capability::Other(not_supported) = capability {
                                    RuntimeError::CapabilityNotSupported(not_supported.clone())
                                } else {
                                    RuntimeError::CapabilityNotAllowed(capability.clone())
                                },
                            ));
                        }
                    }
                }
                Instruction::Error(err) => {
                    return Some(Err(RuntimeError::ScriptErrorMessage(
                        self.eval_string(&err.message).into_owned(),
                    )))
                }
                Instruction::Invalid(invalid) => {
                    return Some(Err(RuntimeError::InvalidInstruction(invalid.clone())));
                }

                #[cfg(test)]
                Instruction::External((command, params)) => {
                    self.pos += 1;
                    return Some(Ok(Event::TestCommand {
                        command: command.to_string(),
                        params: params
                            .iter()
                            .map(|p| self.eval_string(p).to_string())
                            .collect(),
                    }));
                }
            }

            self.pos += 1;
        }

        None
    }

    pub fn set_envelope<'y>(&mut self, envelope: impl Into<Envelope<'x>>, value: &'y str) {
        if let Some(value) = parse_envelope_address(value) {
            self.envelope.push((envelope.into(), value.into()));
        }
    }

    pub fn with_envelope(mut self, envelope: impl Into<Envelope<'x>>, value: &str) -> Self {
        self.set_envelope(envelope, value);
        self
    }

    pub fn clear_envelope(&mut self) {
        self.envelope.clear()
    }
}
