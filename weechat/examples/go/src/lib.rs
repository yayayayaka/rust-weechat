// Copyright (C) 2010 m4v <lambdae2@gmail.com>
// Copyright (C) 2011 stfn <stfnmd@googlemail.com>
// Copyright (C) 2009-2014 Sébastien Helleu <flashcode@flashtux.org>
// Copyright (C) 2020 Damir Jelić <poljar@termina.org.uk>
//
// This program is free software; you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation; either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{borrow::Cow, cell::RefCell, cmp::Ordering, rc::Rc};

use weechat::{
    buffer::Buffer,
    config,
    hooks::{
        Command, CommandCallback, CommandRun, CommandRunCallback,
        CommandSettings, ModifierCallback, ModifierData, ModifierHook,
    },
    infolist::InfolistVariable,
    weechat_plugin, Args, ReturnCode, Weechat, Plugin,
};

use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};

config!(
    "go",
    Section look {
        prompt: String {
            "Prompt to display before the list of buffers.",
            "Go to: ",
        },

        color_name_fg: Color {
            "Foreground color for the non-selected name of a buffer.",
            "black",
        },
        color_name_bg: Color {
            "Background color for the non-selected name of a buffer.",
            "cyan",
        },
        color_name_selected_fg: Color {
            "Foreground color for the selected name of a buffer.",
            "black",
        },
        color_name_selected_bg: Color {
            "Background color for the selected name of a buffer.",
            "yellow",
        },

        color_number_fg: Color {
            "Foreground color for the non-selected number of a buffer.",
            "yellow",
        },
        color_number_bg: Color {
            "Background color for the non-selected number of a buffer.",
            "magenta",
        },
        color_number_selected_fg: Color {
            "Foreground color for the selected number of a buffer.",
            "yellow",
        },
        color_number_selected_bg: Color {
            "Background color for the selected number of a buffer.",
            "red",
        }
    },

    Section behaviour {
        autojump: bool {
            "Automatically jump to a buffer when it is uniquely selected.",
            false,
        }
    }
);

struct Go {
    #[used]
    command: Command,
}

#[derive(Clone)]
struct InnerGo {
    running_state: Rc<RefCell<Option<RunningState>>>,
    config: Rc<Config>,
}

impl InnerGo {
    fn stop(&self, weechat: &Weechat, switch_buffer: bool) {
        self.running_state
            .borrow_mut()
            .take()
            .map(|s| s.stop(weechat, switch_buffer));
    }
}

#[derive(Clone)]
struct InputState {
    input_string: Rc<String>,
    input_position: i32,
}

impl InputState {
    /// Restore the input state on the given buffer.
    fn restore_for_buffer(&self, buffer: &Buffer) {
        buffer.set_input(&self.input_string);
        buffer.set_input_position(self.input_position);
    }
}

impl<'a> From<&'a Buffer<'a>> for InputState {
    fn from(buffer: &Buffer) -> Self {
        InputState {
            input_string: Rc::new(buffer.input().to_string()),
            input_position: buffer.input_position(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd)]
struct BufferData {
    score: i64,
    number: i32,
    full_name: Rc<String>,
    short_name: Rc<String>,
}

impl<'a> From<&Buffer<'a>> for BufferData {
    fn from(buffer: &Buffer) -> Self {
        BufferData {
            score: 0,
            number: buffer.number(),
            full_name: Rc::new(buffer.full_name().to_string()),
            short_name: Rc::new(buffer.short_name().to_string()),
        }
    }
}

impl Ord for BufferData {
    fn cmp(&self, other: &Self) -> Ordering {
        let score = self.score.cmp(&other.score);

        match score {
            Ordering::Equal => self.number.cmp(&other.number),
            _ => score,
        }
    }
}

#[derive(Clone)]
struct BufferList {
    /// The Weechat configuration for this plugin.
    config: Rc<Config>,
    /// The list of buffers, this will first contain all buffers but can be
    /// filtered down with the `filter()` method.
    buffers: Vec<BufferData>,
    /// Index remembering which buffer the user selected. This can be
    /// manipulated using `select_next_buffer()` and `select_prev_buffer()`.
    selected_buffer: usize,
}

impl BufferList {
    /// Create a new buffer list.
    ///
    /// This will fetch all the buffers from the Weechat info-list and set an
    /// initial score of 0 for every buffer.
    fn new(weechat: &Weechat, config: Rc<Config>) -> Self {
        let info_list = weechat
            .get_infolist("buffer", None)
            .expect("Can't get buffer infolist");

        let mut buffers = Vec::new();

        for item in info_list {
            let buffer =
                item.get("pointer").expect("Infolist doesn't have a buffer");

            if let InfolistVariable::Buffer(b) = buffer {
                buffers.push(BufferData::from(&b));
            }
        }

        BufferList {
            config,
            buffers,
            selected_buffer: 0,
        }
    }

    /// Filter our list using a fuzzy matcher with the given pattern.
    ///
    /// Returns a new list of buffers that only contains buffers that match the
    /// given pattern, the score is adjusted to signal how well a buffer matches
    /// the pattern.
    fn filter(&self, pattern: &str) -> Self {
        let matcher = SkimMatcherV2::default();

        let mut buffers: Vec<BufferData> = self
            .buffers
            .iter()
            .filter_map(|buffer_data| {
                matcher.fuzzy_match(&buffer_data.short_name, &pattern).map(
                    |score| {
                        let mut new_buffer = buffer_data.clone();
                        new_buffer.score = score;
                        new_buffer
                    },
                )
            })
            .collect();

        buffers.sort();

        BufferList {
            config: self.config.clone(),
            buffers,
            selected_buffer: 0,
        }
    }

    /// Set the next buffer as our selected buffer.
    ///
    /// This will wrap if we reach the end of our buffer list, e.g. if we're at
    /// the last buffer and call this method the selected buffer will now be the
    /// first buffer.
    fn select_next_buffer(&mut self) {
        self.selected_buffer += 1;

        if self.selected_buffer >= self.buffers.len() {
            self.selected_buffer = 0;
        }
    }

    /// Set the previous buffer as our selected buffer.
    ///
    /// This will wrap if we reach the start of our buffer list, e.g. if we're
    /// at the first buffer and call this method the selected buffer will now
    /// be the last buffer.
    fn select_prev_buffer(&mut self) {
        if self.selected_buffer == 0 {
            self.selected_buffer = if self.buffers.is_empty() {
                0
            } else {
                self.buffers.len() - 1
            };
        } else {
            self.selected_buffer -= 1
        }
    }

    /// Get our selected buffer if there is one.
    fn get_selected_buffer(&self) -> Option<&BufferData> {
        self.buffers.get(self.selected_buffer)
    }

    /// Do we have exactly one result in our buffer list.
    fn has_only_one_result(&self) -> bool {
        self.buffers.len() == 1
    }

    /// Switch to the currently selected buffer.
    ///
    /// # Arguments
    ///
    /// * `weechat` - The Weechat context that will allow us to find the buffer
    ///     object using our full name of the buffer.
    fn switch_to_selected_buffer(self, weechat: &Weechat) {
        self.get_selected_buffer().map(|buffer| {
            weechat
                .buffer_search("==", &buffer.full_name)
                .map(|buffer| {
                    buffer.switch_to();
                });
        });
    }
}

impl std::fmt::Display for BufferList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name_fg = self.config.look().color_name_fg();
        let name_bg = self.config.look().color_name_bg();
        let name_selected_fg = self.config.look().color_name_selected_fg();
        let name_selected_bg = self.config.look().color_name_selected_bg();

        let number_fg = self.config.look().color_number_fg();
        let number_bg = self.config.look().color_number_bg();
        let number_selected_fg = self.config.look().color_number_selected_fg();
        let number_selected_bg = self.config.look().color_number_selected_bg();

        let buffers: Vec<String> = self
            .buffers
            .iter()
            .enumerate()
            .map(|(i, buffer_data)| {
                let number_color = if i == self.selected_buffer {
                    Weechat::color_pair(
                        &number_selected_fg,
                        &number_selected_bg,
                    )
                } else {
                    Weechat::color_pair(&number_fg, &number_bg)
                };

                let name_color = if i == self.selected_buffer {
                    Weechat::color_pair(&name_selected_fg, &name_selected_bg)
                } else {
                    Weechat::color_pair(&name_fg, &name_bg)
                };

                format!(
                    "{}{}{}{}{}",
                    number_color,
                    buffer_data.number,
                    name_color,
                    buffer_data.short_name,
                    Weechat::color("reset"),
                )
            })
            .collect();

        f.write_fmt(format_args!("{}", buffers.join(" ")))
    }
}

struct Hooks {
    #[used]
    modifier: ModifierHook,
    #[used]
    input_command: CommandRun,
    #[used]
    buffer_command: CommandRun,
    #[used]
    window_command: CommandRun,
}

impl Hooks {
    fn new(inner_go: &InnerGo) -> Self {
        // Override our input command.
        let input_command = CommandRun::new("2000|/input *", inner_go.clone())
            .expect("Can't override input command");

        // Disable buffer commands while in go mode.
        let buffer_command = CommandRun::new(
            "2000|/buffer *",
            |_: &Weechat, _: &Buffer, _: Cow<str>| ReturnCode::OkEat,
        )
        .expect("Can't override buffer command");

        // Disable window commands while in go mode.
        let window_command = CommandRun::new(
            "2000|/window *",
            |_: &Weechat, _: &Buffer, _: Cow<str>| ReturnCode::OkEat,
        )
        .expect("Can't override window command");

        // Override our buffer input text so we can display the go buffer line.
        let modifier = ModifierHook::new(
            "input_text_display_with_cursor",
            inner_go.clone(),
        )
        .expect("Can't hook the input text modifier");

        Hooks {
            input_command,
            buffer_command,
            window_command,
            modifier,
        }
    }
}

struct RunningState {
    /// Hooks that are necessary to enable go-mode.
    hooks: Hooks,
    /// The input of the current buffer before we entered go-mode.
    saved_input: InputState,
    /// Our stored input while in go-mode.
    last_input: String,
    /// The current list of buffers we are presenting, will initially contain
    /// all buffers but will get filtered down as we input patterns.
    buffers: BufferList,
}

impl RunningState {
    fn new(inner_go: &InnerGo, weechat: &Weechat, buffer: &Buffer) -> Self {
        RunningState {
            hooks: Hooks::new(inner_go),
            last_input: "".to_owned(),
            saved_input: InputState::from(buffer),
            buffers: BufferList::new(weechat, inner_go.config.clone()),
        }
    }

    /// Stop the interactive go-mode and optionally switch to the currently
    /// selected buffer.
    fn stop(self, weechat: &Weechat, switch_to_buffer: bool) {
        let buffers = self.buffers;
        let saved_input = self.saved_input;

        // We need to drop our hooks first so our callbacks don't run after
        // the state is dropped, that is, setting the input on the buffer
        // will trigger the modifier callback.
        drop(self.hooks);

        let current_buffer = weechat.current_buffer();
        saved_input.restore_for_buffer(&current_buffer);

        if switch_to_buffer {
            buffers.switch_to_selected_buffer(weechat);
        }
    }
}

/// Callback for our modifier hook.
impl ModifierCallback for InnerGo {
    fn callback(
        &mut self,
        weechat: &Weechat,
        _: &str,
        data: Option<ModifierData>,
        string: Cow<str>,
    ) -> Option<String> {
        let buffer = if let ModifierData::Buffer(buffer) = data? {
            if buffer != weechat.current_buffer() {
                return None;
            } else {
                buffer
            }
        } else {
            return None;
        };

        let mut state = self.running_state.borrow_mut();

        let state_borrow = if let Some(state) = state.as_mut() {
            state
        } else {
            // If there's no state anymore we're exiting and the modifier will
            // get unhooked.
            return None;
        };

        // The input line will have some color at the end of the line, remove
        // colors and trim out whitespace at the beginning.
        let current_input = Weechat::remove_color(string.trim_start());

        // If our input changed generate a new buffer list, if the input isn't
        // an empty string filter our buffers with the input.
        if state_borrow.last_input != current_input {
            let buffers = BufferList::new(weechat, self.config.clone());

            let buffers = match current_input.as_ref() {
                "" => buffers,
                _ => buffers.filter(&current_input),
            };

            state_borrow.last_input = current_input.to_string();
            state_borrow.buffers = buffers;
        };

        if state_borrow.buffers.has_only_one_result()
            && self.config.behaviour().autojump()
        {
            buffer
                .run_command("/wait 1ms /input return")
                .expect("Can't run command");
            None
        } else {
            Some(format!(
                "{}{}  {}",
                self.config.look().prompt(),
                string,
                state_borrow.buffers
            ))
        }
    }
}

/// Callback for our `/input` command override.
impl CommandRunCallback for InnerGo {
    fn callback(
        &mut self,
        weechat: &Weechat,
        _: &Buffer,
        command: Cow<str>,
    ) -> ReturnCode {
        if command.starts_with("/input search_text")
            || command.starts_with("/input jump")
        {
            return ReturnCode::OkEat;
        }

        match command.as_ref() {
            "/input return" => {
                self.stop(weechat, true);
                ReturnCode::OkEat
            }
            "/input complete_next" => {
                let mut state = self.running_state.borrow_mut();
                state.as_mut().map(|s| s.buffers.select_next_buffer());
                Weechat::hook_signal_send("input_text_changed", "");

                ReturnCode::OkEat
            }
            "/input complete_previous" => {
                let mut state = self.running_state.borrow_mut();
                state.as_mut().map(|s| s.buffers.select_prev_buffer());
                Weechat::hook_signal_send("input_text_changed", "");

                ReturnCode::OkEat
            }
            _ => ReturnCode::Ok,
        }
    }
}

/// Callback for our `/go` command.
impl CommandCallback for InnerGo {
    fn callback(
        &mut self,
        weechat: &Weechat,
        buffer: &Buffer,
        mut arguments: Args,
    ) {
        if self.running_state.borrow().is_none() {
            // Skip our "/go" command in the argument list.
            arguments.next();
            let mut arguments = arguments.peekable();

            // If there is an argument use the rest of the arguments as the
            // pattern to find a buffer and switch to one if one is found,
            // otherwise start the interactive go-mode.
            if arguments.peek().is_some() {
                let pattern = arguments.collect::<Vec<String>>().join(" ");
                BufferList::new(weechat, self.config.clone())
                    .filter(&pattern)
                    .switch_to_selected_buffer(weechat);
            } else {
                *self.running_state.borrow_mut() =
                    Some(RunningState::new(self, weechat, buffer));
                buffer.set_input("");
            }
        } else {
            self.stop(weechat, false);
        }
    }
}

impl Plugin for Go {
    fn init(_: &Weechat, _args: Args) -> Result<Self, ()> {
        let config = Config::new()?;

        if let Err(e) = config.read() {
            Weechat::print(&format!(
                "{}Error reading go config file {:?}",
                Weechat::prefix("error"),
                e
            ));
            return Err(());
        }

        let inner_go = InnerGo {
            running_state: Rc::new(RefCell::new(None)),
            config: Rc::new(config),
        };

        let command_settings = CommandSettings::new("go")
            .description("Quickly jump to a buffer using fuzzy search.")
            .add_argument("[name]")
            .arguments_description(
                "name: directly jump to a buffer by name (without this \
                argument an interactive mode is entered).\n\n\

                You can bind this command to a key, for example:\n    \
                /key bind meta-g /go\n\n\

                You can use tab completion to select the next/previous buffer \
                in the interactive go-mode.",
            );
        let command = Command::new(command_settings, inner_go)?;

        Ok(Go { command })
    }
}

weechat_plugin!(
    Go,
    name: "go",
    author: "Damir Jelić <poljar@termina.org.uk>",
    description: "Quickly jump to buffers using fuzzy search",
    version: "0.1.0",
    license: "GPL3"
);
