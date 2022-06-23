use std::error::Error;
use std::io::Write;
use std::num::NonZeroU32;
use std::result::Result;
use std::{iter, mem};

use bxt_strafe::{Parameters, State, Trace};
use hltas::types::*;
use hltas::HLTAS;
use rand::distributions::Uniform;
use rand::prelude::Distribution;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tap::{Conv, Pipe, Tap, TryConv};

use super::objective::{AttemptResult, Objective};
use super::remote;
use super::simulator::Simulator;
use crate::modules::triangle_drawing::triangle_api::{Primitive, RenderMode};
use crate::modules::triangle_drawing::TriangleApi;

/// A movement frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    /// Parameters used for simulating this frame.
    pub parameters: Parameters,

    /// Final state after this frame.
    pub state: State,
}

pub struct Editor {
    /// The first part of the script that we're not editing.
    prefix: HLTAS,

    /// The script being edited.
    hltas: HLTAS,

    /// Movement frames, starting from the initial frame.
    frames: Vec<Frame>,

    /// Movement frames from the last mutation, starting from the initial frame.
    last_mutation_frames: Option<Vec<Frame>>,

    /// Generation of this script for remote simulation.
    generation: u16,

    /// Temperature for use in the simulated annealing algorithm.
    temperature: f32,

    /// The exponential decay constant for the temperature.
    cooling_rate: f32,

    /// The number of iterations per temperature.
    max_iterations: usize,

    /// The current number of iterations that have occured.
    current_iterations: usize,
}

trait HLTASExt {
    /// Returns the line index and the repeat for the given `frame`.
    ///
    /// Returns [`None`] if `frame` is bigger than the number of frames in the [`HLTAS`].
    fn line_and_repeat_at_frame(&self, frame: usize) -> Option<(usize, u32)>;

    /// Splits the [`HLTAS`] at `frame` if needed and returns a reference to the frame bulk that
    /// starts at `frame`.
    ///
    /// Returns [`None`] if `frame` is bigger than the number of frames in the [`HLTAS`].
    fn split_at_frame(&mut self, frame: usize) -> Option<&mut FrameBulk>;

    /// Splits the [`HLTAS`] at `frame` if needed and returns a reference to the frame bulk that
    /// starts at `frame` and lasts a single repeat.
    ///
    /// Returns [`None`] if `frame` is bigger than the number of frames in the [`HLTAS`].
    fn split_single_at_frame(&mut self, frame: usize) -> Option<&mut FrameBulk>;
}

impl HLTASExt for HLTAS {
    fn line_and_repeat_at_frame(&self, frame: usize) -> Option<(usize, u32)> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(l, line)| {
                if let Line::FrameBulk(frame_bulk) = line {
                    Some((l, frame_bulk))
                } else {
                    None
                }
            })
            .flat_map(|(l, frame_bulk)| (0..frame_bulk.frame_count.get()).map(move |r| (l, r)))
            .nth(frame)
    }

    fn split_at_frame(&mut self, frame: usize) -> Option<&mut FrameBulk> {
        let (l, r) = self.line_and_repeat_at_frame(frame)?;

        let mut frame_bulk = if let Line::FrameBulk(frame_bulk) = &mut self.lines[l] {
            frame_bulk
        } else {
            unreachable!()
        };

        let index = if r == 0 {
            // The frame bulk already starts here.
            l
        } else {
            let mut new_frame_bulk = frame_bulk.clone();
            new_frame_bulk.frame_count = NonZeroU32::new(frame_bulk.frame_count.get() - r).unwrap();
            frame_bulk.frame_count = NonZeroU32::new(r).unwrap();

            self.lines.insert(l + 1, Line::FrameBulk(new_frame_bulk));

            l + 1
        };

        if let Line::FrameBulk(frame_bulk) = &mut self.lines[index] {
            Some(frame_bulk)
        } else {
            unreachable!()
        }
    }

    fn split_single_at_frame(&mut self, frame: usize) -> Option<&mut FrameBulk> {
        self.split_at_frame(frame + 1);
        self.split_at_frame(frame)
    }
}

impl Editor {
    pub fn new(
        mut hltas: HLTAS,
        first_frame: usize,
        initial_frame: Frame,
        generation: u16,
        temperature: f32,
        cooling_rate: f32,
        max_iterations: usize,
        current_iterations: usize,
    ) -> Self {
        let (l, _r) = hltas.line_and_repeat_at_frame(first_frame).unwrap();

        let mut prefix = hltas.clone();
        prefix.lines.truncate(l);

        hltas.lines = hltas.lines[l..].to_vec();

        // Erase the console command that contains bxt_tas_optim_init.
        //
        // This is so when the single-frame mutation mode splits that frame bulk, it does not lead
        // to bxt_tas_optim_init and other unwanted commands running in the remote client.
        match &mut hltas.lines[0] {
            Line::FrameBulk(frame_bulk) => frame_bulk.console_command = None,
            _ => unreachable!(),
        }

        Self {
            prefix,
            hltas,
            frames: vec![initial_frame],
            last_mutation_frames: None,
            generation,
            temperature,
            cooling_rate,
            max_iterations,
            current_iterations,
        }
    }

    pub fn draw(&self, tri: &TriangleApi) {
        tri.render_mode(RenderMode::TransColor);
        tri.color(0., 1., 1., 1.);

        tri.begin(Primitive::Lines);

        for pair in self.frames.windows(2) {
            let (prev, next) = (&pair[0], &pair[1]);

            tri.vertex(prev.state.player().pos);
            tri.vertex(next.state.player().pos);
        }

        if let Some(frames) = &self.last_mutation_frames {
            tri.color(0., 0.5, 0.5, 1.);

            for pair in frames.windows(2) {
                let (prev, next) = (&pair[0], &pair[1]);

                tri.vertex(prev.state.player().pos);
                tri.vertex(next.state.player().pos);
            }
        }

        tri.end();
    }

    pub fn save<W: Write>(&mut self, writer: W) -> Result<(), Box<dyn Error>> {
        let len = self.prefix.lines.len();
        self.prefix.lines.extend(self.hltas.lines.iter().cloned());
        let rv = self.prefix.to_writer(writer);
        self.prefix.lines.truncate(len);
        Ok(rv?)
    }

    pub fn simulate_all<T: Trace>(&mut self, tracer: &T) {
        let simulator = Simulator::new(tracer, &self.frames, &self.hltas.lines);
        self.frames.extend(simulator);
    }

    pub fn optimize<'a, T: Trace>(
        &'a mut self,
        tracer: &'a T,
        frames: usize,
        random_frames_to_change: usize,
        change_single_frames: bool,
        objective: &'a Objective,
    ) -> Option<impl Iterator<Item = AttemptResult> + 'a> {
        self.simulate_all(tracer);

        if self.frames.len() == 1 {
            return None;
        }

        let mut high = self.frames.len() - 1;
        if frames > 0 {
            high = high.min(frames);
        }

        let between = Uniform::from(0..high);
        let mut rng = rand::thread_rng();

        Some(iter::from_fn(move || {
            let mut hltas = self.hltas.clone();

            // Change several frames.
            let mut stale_frame = self.frames.len() - 1;
            for _ in 0..random_frames_to_change {
                let frame = if change_single_frames {
                    // Pick a random frame and mutate it.
                    let frame = between.sample(&mut rng);
                    mutate_frame(&mut rng, &mut hltas, frame);
                    frame
                } else {
                    mutate_single_frame_bulk(&mut hltas, &mut rng)
                };

                stale_frame = stale_frame.min(frame);
            }

            let mut frames = Vec::from(&self.frames[..stale_frame + 1]);

            // Simulate the result.
            let simulator = Simulator::new(tracer, &frames, &hltas.lines);
            frames.extend(simulator);

            self.current_iterations += 1;

            // Check if we got an improvement.
            let result = objective.eval(&frames, &self.frames);

            match result {
                AttemptResult::Better { .. } => {
                    self.hltas = hltas;
                    self.frames = frames;
                    Some(result)
                }
                AttemptResult::Worse { difference } => {
                    let acceptance: f32 = (difference / self.temperature).exp();
                    assert!(acceptance <= 1_f32);

                    if rng.gen::<f32>() < acceptance {
                        self.hltas = hltas;
                        self.frames = frames;
                    } else {
                        self.last_mutation_frames = Some(frames);
                    }
                    Some(result)
                }
                AttemptResult::Invalid { .. } => {
                    self.last_mutation_frames = Some(frames);
                    Some(result)
                }
            }
        }))
    }

    fn prepare_hltas_for_sending(&mut self) -> HLTAS {
        let len = self.prefix.lines.len();
        self.prefix.lines.extend(self.hltas.lines.iter().cloned());

        // Replace the TAS editor / TAS optim commands with the start sending frames command.
        match &mut self.prefix.lines[len] {
            Line::FrameBulk(frame_bulk) => {
                frame_bulk.console_command =
                    Some("_bxt_tas_optim_simulation_start_recording_frames".to_owned());
            }
            _ => unreachable!(),
        }

        // Add a toggleconsole command in the end.
        self.prefix.lines.push(Line::FrameBulk(
            FrameBulk::with_frame_time("0.001".to_owned()).tap_mut(|x| {
                x.console_command = Some("_bxt_tas_optim_simulation_done;toggleconsole".to_owned())
            }),
        ));

        let hltas = self.prefix.clone();
        self.prefix.lines.truncate(len);
        hltas
    }

    pub fn maybe_simulate_all_in_remote_client(&mut self) {
        if self.frames.len() > 1 {
            // Already simulated.
            return;
        }

        // Check if we have already been simulating and it has finished.
        remote::receive_simulation_result_from_clients(|_hltas, generation, mut frames| {
            if generation != self.generation {
                return;
            }

            frames.insert(0, mem::take(&mut self.frames).into_iter().next().unwrap());
            self.frames = frames;
        });

        if self.frames.len() > 1 {
            // Already simulated.
            return;
        }

        if !remote::is_any_client_simulating_generation(self.generation) {
            // Try to send the script for simulation.
            remote::maybe_simulate_in_one_client(|| {
                (self.prepare_hltas_for_sending(), self.generation)
            });
        }
    }

    pub fn poll_remote_clients_when_not_optimizing(&mut self) {
        remote::receive_simulation_result_from_clients(|_hltas, generation, mut frames| {
            if generation != self.generation {
                return;
            }

            if self.frames.len() == 1 {
                // Received the initial simulation result, don't miss it.
                frames.insert(0, mem::take(&mut self.frames).into_iter().next().unwrap());
                self.frames = frames;
            }

            // Otherwise, we have received simulation result that we no longer care about.
        });
    }

    // Yes I know this is not the best structured code at the moment...
    #[allow(clippy::too_many_arguments)]
    pub fn optimize_with_remote_clients(
        &mut self,
        frames: usize,
        random_frames_to_change: usize,
        change_single_frames: bool,
        objective: &Objective,
        mut on_improvement: impl FnMut(&str),
    ) {
        self.maybe_simulate_all_in_remote_client();

        if self.frames.len() == 1 {
            // Haven't finished the initial simulation yet...
            return;
        }

        remote::receive_simulation_result_from_clients(|mut hltas, generation, mut frames| {
            if generation != self.generation {
                return;
            }

            frames.insert(0, self.frames[0].clone());
            self.last_mutation_frames = Some(frames.clone());

            if let AttemptResult::Better { value } = objective.eval(&frames, &self.frames) {
                self.hltas.lines = hltas
                    .lines
                    .drain(self.prefix.lines.len()..hltas.lines.len() - 1)
                    .collect();

                // Remove the start sending frames command.
                match &mut self.hltas.lines[0] {
                    Line::FrameBulk(frame_bulk) => frame_bulk.console_command = None,
                    _ => unreachable!(),
                };

                self.frames = frames;
                on_improvement(&value);
            }
        });

        let mut high = self.frames.len() - 1;
        if frames > 0 {
            high = high.min(frames);
        }

        let between = Uniform::from(0..high);
        let mut rng = rand::thread_rng();

        remote::simulate_in_available_clients(|| {
            let temp = self.hltas.clone();

            // Change several frames.
            for _ in 0..random_frames_to_change {
                if change_single_frames {
                    let frame = between.sample(&mut rng);
                    let frame_bulk = self.hltas.split_single_at_frame(frame).unwrap();
                    mutate_frame_bulk(&mut rng, frame_bulk);
                } else {
                    mutate_single_frame_bulk(&mut self.hltas, &mut rng);
                }
            }

            let hltas = self.prepare_hltas_for_sending();

            self.hltas = temp;

            (hltas, self.generation)
        });
    }

    pub fn minimize<T: Trace>(&mut self, tracer: &T) {
        // Remove unused keys and actions.
        let mut state = self.frames[0].state.clone();
        let mut parameters = self.frames[0].parameters;
        let mut preferred_leave_ground_action_type = LeaveGroundActionType::Jump;

        for line in &mut self.hltas.lines {
            if let Line::FrameBulk(frame_bulk) = line {
                if let Some(action) = frame_bulk.auto_actions.leave_ground_action {
                    preferred_leave_ground_action_type = action.type_;
                }

                parameters.frame_time =
                    (frame_bulk.frame_time.parse::<f32>().unwrap_or(0.) * 1000.).trunc() / 1000.;

                let simulate = |frame_bulk: &FrameBulk| {
                    let mut state_new = state.clone();
                    for _ in 0..frame_bulk.frame_count.get() {
                        state_new = state_new.clone().simulate(tracer, parameters, frame_bulk).0;
                    }
                    state_new
                };

                let mut state_original = simulate(frame_bulk);

                if frame_bulk.action_keys.use_ {
                    frame_bulk.action_keys.use_ = false;
                    let state_new = simulate(frame_bulk);
                    if state_original.player() == state_new.player() {
                        state_original = state_new;
                    } else {
                        frame_bulk.action_keys.use_ = true;
                    }
                }

                if let Some(action) = frame_bulk.auto_actions.leave_ground_action {
                    frame_bulk.auto_actions.leave_ground_action = Some(LeaveGroundAction {
                        speed: LeaveGroundActionSpeed::Optimal,
                        times: Times::UnlimitedWithinFrameBulk,
                        type_: preferred_leave_ground_action_type,
                    });
                    let state_new = simulate(frame_bulk);
                    if state_original.player() == state_new.player() {
                        state_original = state_new;
                    } else {
                        frame_bulk.auto_actions.leave_ground_action = Some(action);
                    }
                }

                if let Some(action) = frame_bulk.auto_actions.duck_before_ground {
                    frame_bulk.auto_actions.duck_before_ground = None;
                    let state_new = simulate(frame_bulk);
                    if state_original.player() == state_new.player() {
                        state_original = state_new;
                    } else {
                        frame_bulk.auto_actions.duck_before_ground = Some(action);
                    }
                }

                state = state_original;
            }

            match line {
                Line::FrameBulk(_) => (),
                Line::Save(_) => (),
                Line::SharedSeed(_) => (),
                Line::Buttons(_) => (),
                Line::LGAGSTMinSpeed(_) => (),
                Line::Reset { non_shared_seed: _ } => (),
                Line::Comment(_) => (),
                Line::VectorialStrafing(_) => (),
                Line::VectorialStrafingConstraints(_) => (),
                Line::Change(_) => (),
                Line::TargetYawOverride(_) => (),
            }
        }

        // Join split frame bulks.
        let mut i = 0;
        let lines = &self.hltas.lines;
        let mut new_lines = Vec::new();

        while i < lines.len() {
            match &lines[i] {
                Line::FrameBulk(frame_bulk) => {
                    let mut frame_bulk = frame_bulk.clone();

                    let mut j = i;
                    loop {
                        j += 1;
                        if j == lines.len() {
                            break;
                        }

                        match &lines[j] {
                            Line::FrameBulk(next_frame_bulk) => {
                                let frame_count = frame_bulk.frame_count;
                                frame_bulk.frame_count = next_frame_bulk.frame_count;
                                if &frame_bulk == next_frame_bulk {
                                    frame_bulk.frame_count = NonZeroU32::new(
                                        frame_count.get() + next_frame_bulk.frame_count.get(),
                                    )
                                    .unwrap();
                                } else {
                                    frame_bulk.frame_count = frame_count;
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }

                    new_lines.push(Line::FrameBulk(frame_bulk));
                    i = j;
                }
                line => {
                    new_lines.push(line.clone());
                    i += 1;
                }
            }
        }

        self.hltas.lines = new_lines;
    }

    pub fn update_temperature(&mut self) {
        if self.current_iterations > self.max_iterations {
            self.temperature *= self.cooling_rate;
            eprintln!("Optim: Temperature = {:.4}", self.temperature);
            self.current_iterations = 0;
        }
    }

    pub fn get_temperature(&self) -> f32 {
        self.temperature
    }
}

fn mutate_frame<R: Rng>(rng: &mut R, hltas: &mut HLTAS, frame: usize) {
    if frame > 0 {
        let l = hltas.line_and_repeat_at_frame(frame).unwrap().0;
        let frame_bulk = hltas.split_at_frame(frame).unwrap();
        if l == 0 {
            // If we split the first frame bulk, empty out the console command (which contains
            // optim init and TAS editor commands).
            frame_bulk.console_command = None;
        }
    }

    // Split it into its own frame bulk.
    let frame_bulk = hltas.split_single_at_frame(frame).unwrap();

    mutate_frame_bulk(rng, frame_bulk);
}

fn mutate_frame_bulk<R: Rng>(rng: &mut R, frame_bulk: &mut FrameBulk) {
    let p = rng.gen::<f32>();
    let strafe_type = if p < 0.01 {
        StrafeType::MaxDeccel
    } else if p < 0.1 {
        StrafeType::MaxAngle
    } else {
        StrafeType::MaxAccel
    };
    frame_bulk.auto_actions.movement = Some(AutoMovement::Strafe(StrafeSettings {
        type_: strafe_type,
        dir: if strafe_type == StrafeType::MaxDeccel {
            StrafeDir::Best
        } else if rng.gen::<bool>() {
            StrafeDir::Left
        } else {
            StrafeDir::Right
        },
    }));

    mutate_action_keys(rng, frame_bulk);
    mutate_auto_actions(rng, frame_bulk);
}

fn mutate_single_frame_bulk<R: Rng>(hltas: &mut HLTAS, rng: &mut R) -> usize {
    let count = hltas
        .lines
        .iter()
        .filter(|line| matches!(line, Line::FrameBulk(..)))
        .count();

    let index = rng.gen_range(0..count);

    let frame_bulk = hltas
        .lines
        .iter_mut()
        .filter_map(|line| {
            if let Line::FrameBulk(frame_bulk) = line {
                Some(frame_bulk)
            } else {
                None
            }
        })
        .nth(index)
        .unwrap();

    if let Some(AutoMovement::Strafe(StrafeSettings { type_, dir, .. })) =
        frame_bulk.auto_actions.movement.as_mut()
    {
        // Mutate strafe type.
        let p = rng.gen::<f32>();
        *type_ = if p < 0.01 {
            StrafeType::MaxDeccel
        } else if p < 0.1 {
            StrafeType::MaxAngle
        } else {
            StrafeType::MaxAccel
        };

        // Mutate strafe direction.
        match dir {
            StrafeDir::Yaw(yaw) => {
                *yaw += if rng.gen::<f32>() < 0.05 {
                    rng.gen_range(-180f32..180f32)
                } else {
                    rng.gen_range(-1f32..1f32)
                };
            }
            StrafeDir::LeftRight(count) | StrafeDir::RightLeft(count) => {
                *count = NonZeroU32::new(
                    (count.get().conv::<i64>() + rng.gen_range(-10..10))
                        .max(1)
                        .min(u32::MAX.into())
                        .try_conv()
                        .unwrap(),
                )
                .unwrap();

                if rng.gen::<f32>() < 0.05 {
                    // Invert the strafe dir.
                    let count = *count;
                    *dir = if matches!(*dir, StrafeDir::LeftRight(_)) {
                        StrafeDir::RightLeft(count)
                    } else {
                        StrafeDir::LeftRight(count)
                    };
                }
            }
            StrafeDir::Left | StrafeDir::Right => {
                if rng.gen::<f32>() < 0.05 {
                    // Invert the strafe dir.
                    *dir = if *dir == StrafeDir::Left {
                        StrafeDir::Right
                    } else {
                        StrafeDir::Left
                    };
                }
            }
            _ => (),
        }
    }

    mutate_action_keys(rng, frame_bulk);
    mutate_auto_actions(rng, frame_bulk);

    // Mutate frame count.
    if index + 1 < count {
        let frame_time = frame_bulk.frame_time.clone();

        let next_frame_bulk = hltas
            .lines
            .iter_mut()
            .filter_map(|line| {
                if let Line::FrameBulk(frame_bulk) = line {
                    Some(frame_bulk)
                } else {
                    None
                }
            })
            .nth(index + 1)
            .unwrap();

        // Can only move the boundary between frame bulks if the frame times match.
        if frame_time == next_frame_bulk.frame_time {
            // Can't go below frame count of 1 on the next frame bulk.
            let max_frame_count_difference = (next_frame_bulk.frame_count.get() - 1)
                .conv::<i64>()
                .min(10);

            // Can't go above frame count of u32::MAX on the next frame bulk.
            let min_frame_count_difference =
                (next_frame_bulk.frame_count.get().conv::<i64>() - u32::MAX.conv::<i64>()).max(-10);

            let frame_count_difference_range =
                min_frame_count_difference..max_frame_count_difference;

            let frame_bulk = hltas
                .lines
                .iter_mut()
                .filter_map(|line| {
                    if let Line::FrameBulk(frame_bulk) = line {
                        Some(frame_bulk)
                    } else {
                        None
                    }
                })
                .nth(index)
                .unwrap();

            let difference = frame_bulk.frame_count.pipe_ref_mut(|count| {
                let orig_count = count.get();

                *count = NonZeroU32::new(
                    (count.get().conv::<i64>() + rng.gen_range(frame_count_difference_range))
                        .max(1)
                        .min(u32::MAX.into())
                        .try_conv()
                        .unwrap(),
                )
                .unwrap();

                orig_count.conv::<i64>() - count.get().conv::<i64>()
            });

            let next_frame_bulk = hltas
                .lines
                .iter_mut()
                .filter_map(|line| {
                    if let Line::FrameBulk(frame_bulk) = line {
                        Some(frame_bulk)
                    } else {
                        None
                    }
                })
                .nth(index + 1)
                .unwrap();

            next_frame_bulk.frame_count.pipe_ref_mut(|count| {
                *count =
                    NonZeroU32::new((count.get().conv::<i64>() + difference).try_conv().unwrap())
                        .unwrap()
            });
        }
    }

    let frame = hltas
        .lines
        .iter_mut()
        .filter_map(|line| {
            if let Line::FrameBulk(frame_bulk) = line {
                Some(frame_bulk)
            } else {
                None
            }
        })
        .take(index)
        .map(|frame_bulk| frame_bulk.frame_count.get().try_conv::<usize>().unwrap())
        .sum();

    frame
}

fn mutate_action_keys<R: Rng>(rng: &mut R, frame_bulk: &mut FrameBulk) {
    if rng.gen::<f32>() < 0.05 {
        frame_bulk.action_keys.use_ = !frame_bulk.action_keys.use_;
    }
}

fn mutate_auto_actions<R: Rng>(rng: &mut R, frame_bulk: &mut FrameBulk) {
    if rng.gen::<f32>() < 0.05 {
        frame_bulk.auto_actions.duck_before_ground =
            if frame_bulk.auto_actions.duck_before_ground.is_some() {
                None
            } else {
                Some(DuckBeforeGround {
                    times: Times::UnlimitedWithinFrameBulk,
                })
            };
    }

    if rng.gen::<f32>() < 0.1 {
        let p = rng.gen::<f32>();
        frame_bulk.auto_actions.leave_ground_action = if p < 1. / 3. {
            None
        } else if p < 2. / 3. {
            Some(LeaveGroundAction {
                speed: if rng.gen::<bool>() {
                    LeaveGroundActionSpeed::Any
                } else {
                    LeaveGroundActionSpeed::Optimal
                },
                times: Times::UnlimitedWithinFrameBulk,
                type_: LeaveGroundActionType::DuckTap {
                    zero_ms: frame_bulk.frame_time == "0.001",
                },
            })
        } else {
            Some(LeaveGroundAction {
                speed: if rng.gen::<bool>() {
                    LeaveGroundActionSpeed::Any
                } else {
                    LeaveGroundActionSpeed::Optimal
                },
                times: Times::UnlimitedWithinFrameBulk,
                type_: LeaveGroundActionType::Jump,
            })
        };
    }
}

// proptest: after simulating, self.frames.len() = frame count + 1
