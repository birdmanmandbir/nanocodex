use std::{
    collections::VecDeque,
    sync::{Arc, mpsc as std_mpsc},
    thread,
    time::{Duration, Instant},
};

use mlx_whisper_rs::{
    audio::audio_from_pcm_s16le,
    load_models::load_model,
    transcribe::{TranscribeOptions, transcribe},
};
use nanocodex::oai::realtime::{REALTIME_SAMPLE_RATE, RealtimeAudio};
use tokio::sync::{mpsc, oneshot};

use super::{
    MeetingEvent, MeetingFailure, MeetingSource, MeetingTranscription, audio::MicrophoneCapture,
    audio::SystemCapture, send_event,
};
use crate::audio::LinearResampler;

const WHISPER_SAMPLE_RATE: u32 = 16_000;
const INFERENCE_INTERVAL: Duration = Duration::from_secs(2);
const FINAL_SILENCE: Duration = Duration::from_millis(900);
const MAX_UTTERANCE: Duration = Duration::from_secs(24);
const PRE_ROLL: Duration = Duration::from_millis(400);
const SPEECH_RMS_THRESHOLD: f32 = 200.0;
const COMMAND_CAPACITY: usize = 2;

const MULTILINGUAL_TOKENIZER: &[u8] = include_bytes!("../assets/whisper/multilingual.tiktoken");
const MEL_FILTERS_80: &[u8] = include_bytes!("../assets/whisper/mel_filters_80.npy");
const MEL_FILTERS_128: &[u8] = include_bytes!("../assets/whisper/mel_filters_128.npy");

pub(super) async fn run(
    model: Arc<str>,
    events: &mpsc::UnboundedSender<MeetingEvent>,
    mut stopped: oneshot::Receiver<()>,
) -> Result<(), MeetingFailure> {
    send_event(events, MeetingEvent::Connecting);
    let mut worker = MlxWorker::spawn(model).await?;

    let (microphone_capture, mut microphone) = MicrophoneCapture::open()?;
    let (mut system_capture, mut system_audio) = match SystemCapture::open().await {
        Ok((capture, audio)) => (Some(capture), Some(audio)),
        Err(error) => {
            send_event(
                events,
                MeetingEvent::Degraded {
                    source: MeetingSource::System,
                    error: error.to_string(),
                },
            );
            (None, None)
        }
    };
    send_event(
        events,
        MeetingEvent::Started {
            system_audio: system_capture.is_some(),
            transcription: MeetingTranscription::Mlx,
        },
    );

    let mut microphone_state = SourceState::new();
    let mut system_state = SourceState::new();
    let mut inference_tick = tokio::time::interval(Duration::from_millis(100));
    inference_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = loop {
        tokio::select! {
            _ = &mut stopped => break Ok(()),
            frame = microphone.recv() => {
                let Some(frame) = frame else {
                    break Err(MeetingFailure::StreamStopped("microphone capture stopped"));
                };
                microphone_state.push(frame);
            }
            frame = async {
                match &mut system_audio {
                    Some(audio) => audio.recv().await,
                    None => futures_util::future::pending().await,
                }
            } => {
                let Some(frame) = frame else {
                    send_event(
                        events,
                        MeetingEvent::Degraded {
                            source: MeetingSource::System,
                            error: "system-audio capture stopped".to_owned(),
                        },
                    );
                    system_capture = None;
                    system_audio = None;
                    continue;
                };
                system_state.push(frame);
            }
            result = worker.results.recv() => {
                let Some(result) = result else {
                    break Err(MeetingFailure::Mlx("inference worker stopped unexpectedly".to_owned()));
                };
                apply_inference_result(
                    result,
                    events,
                    &mut microphone_state,
                    &mut system_state,
                )?;
            }
            _ = inference_tick.tick() => {
                let now = Instant::now();
                microphone_state.maybe_submit(MeetingSource::Microphone, now, &worker.commands);
                system_state.maybe_submit(MeetingSource::System, now, &worker.commands);
            }
        }
    };

    drop(microphone_capture);
    drop(system_capture);
    if result.is_ok() {
        flush_sources(
            events,
            &mut worker,
            &mut microphone_state,
            &mut system_state,
        )
        .await?;
    }
    worker.shutdown()?;
    result
}

async fn flush_sources(
    events: &mpsc::UnboundedSender<MeetingEvent>,
    worker: &mut MlxWorker,
    microphone: &mut SourceState,
    system: &mut SourceState,
) -> Result<(), MeetingFailure> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        microphone.request_final(MeetingSource::Microphone, &worker.commands);
        system.request_final(MeetingSource::System, &worker.commands);
        if !microphone.has_pending_audio() && !system.has_pending_audio() {
            return Ok(());
        }
        let result = tokio::time::timeout_at(deadline, worker.results.recv())
            .await
            .map_err(|_| MeetingFailure::Mlx("timed out flushing local transcript".to_owned()))?
            .ok_or_else(|| {
                MeetingFailure::Mlx("inference worker stopped while flushing".to_owned())
            })?;
        apply_inference_result(result, events, microphone, system)?;
    }
}

fn apply_inference_result(
    result: InferenceResult,
    events: &mpsc::UnboundedSender<MeetingEvent>,
    microphone: &mut SourceState,
    system: &mut SourceState,
) -> Result<(), MeetingFailure> {
    let state = match result.source {
        MeetingSource::Microphone => microphone,
        MeetingSource::System => system,
    };
    state.in_flight = false;
    if let Some(error) = result.error {
        return Err(MeetingFailure::Mlx(error));
    }
    if result.generation != state.generation {
        return Ok(());
    }
    let text = result.text.trim();
    if result.final_batch {
        for text in state.hypothesis.finish(text) {
            send_event(
                events,
                MeetingEvent::TranscriptFinal {
                    source: result.source,
                    text,
                },
            );
        }
        send_event(
            events,
            MeetingEvent::TranscriptPartial {
                source: result.source,
                text: String::new(),
            },
        );
        state.reset_utterance();
    } else {
        let update = state.hypothesis.observe(text);
        for text in update.finals {
            send_event(
                events,
                MeetingEvent::TranscriptFinal {
                    source: result.source,
                    text,
                },
            );
        }
        send_event(
            events,
            MeetingEvent::TranscriptPartial {
                source: result.source,
                text: update.partial,
            },
        );
    }
    Ok(())
}

struct SourceState {
    resampler: LinearResampler,
    resampled: Vec<f32>,
    pre_roll: VecDeque<i16>,
    samples: Vec<i16>,
    active: bool,
    last_speech: Option<Instant>,
    last_submitted_samples: usize,
    in_flight: bool,
    final_in_flight: bool,
    generation: u64,
    hypothesis: RollingHypothesis,
}

impl SourceState {
    fn new() -> Self {
        Self {
            resampler: LinearResampler::new(REALTIME_SAMPLE_RATE, WHISPER_SAMPLE_RATE),
            resampled: Vec::new(),
            pre_roll: VecDeque::with_capacity(duration_samples(PRE_ROLL)),
            samples: Vec::new(),
            active: false,
            last_speech: None,
            last_submitted_samples: 0,
            in_flight: false,
            final_in_flight: false,
            generation: 0,
            hypothesis: RollingHypothesis::default(),
        }
    }

    fn push(&mut self, audio: RealtimeAudio) {
        let source = audio
            .as_bytes()
            .chunks_exact(2)
            .map(|sample| i16::from_le_bytes([sample[0], sample[1]]));
        let source = source.collect::<Vec<_>>();
        let speech = rms(&source) >= SPEECH_RMS_THRESHOLD;
        self.resampler.push_into(
            source
                .iter()
                .map(|sample| f32::from(*sample) / f32::from(i16::MAX)),
            &mut self.resampled,
        );
        let converted = self
            .resampled
            .iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16);

        if self.active {
            self.samples.extend(converted);
            if speech {
                if self.final_in_flight {
                    self.generation = self.generation.saturating_add(1);
                    self.final_in_flight = false;
                }
                self.last_speech = Some(Instant::now());
            }
        } else {
            self.pre_roll.extend(converted);
            while self.pre_roll.len() > duration_samples(PRE_ROLL) {
                let _ = self.pre_roll.pop_front();
            }
            if speech {
                self.active = true;
                self.last_speech = Some(Instant::now());
                self.samples.extend(self.pre_roll.drain(..));
            }
        }
    }

    fn maybe_submit(
        &mut self,
        source: MeetingSource,
        now: Instant,
        commands: &std_mpsc::SyncSender<InferenceRequest>,
    ) {
        if !self.active || self.in_flight || self.samples.len() < WHISPER_SAMPLE_RATE as usize {
            return;
        }
        let final_batch = self.last_speech.is_some_and(|speech| {
            now.duration_since(speech) >= FINAL_SILENCE
                || self.samples.len() >= duration_samples(MAX_UTTERANCE)
        });
        let due = self
            .samples
            .len()
            .saturating_sub(self.last_submitted_samples)
            >= duration_samples(INFERENCE_INTERVAL);
        if final_batch || due {
            self.submit(source, final_batch, commands);
        }
    }

    fn request_final(
        &mut self,
        source: MeetingSource,
        commands: &std_mpsc::SyncSender<InferenceRequest>,
    ) {
        if self.in_flight || !self.has_pending_audio() {
            return;
        }
        self.submit(source, true, commands);
    }

    fn submit(
        &mut self,
        source: MeetingSource,
        final_batch: bool,
        commands: &std_mpsc::SyncSender<InferenceRequest>,
    ) {
        let request = InferenceRequest {
            source,
            generation: self.generation,
            samples: self.samples.clone(),
            final_batch,
        };
        if commands.try_send(request).is_ok() {
            self.last_submitted_samples = self.samples.len();
            self.in_flight = true;
            self.final_in_flight = final_batch;
        }
    }

    const fn has_pending_audio(&self) -> bool {
        self.active && !self.samples.is_empty()
    }

    fn reset_utterance(&mut self) {
        self.samples.clear();
        self.pre_roll.clear();
        self.active = false;
        self.last_speech = None;
        self.last_submitted_samples = 0;
        self.final_in_flight = false;
        self.generation = self.generation.saturating_add(1);
        self.hypothesis = RollingHypothesis::default();
    }
}

fn duration_samples(duration: Duration) -> usize {
    (duration.as_secs_f64() * f64::from(WHISPER_SAMPLE_RATE)) as usize
}

fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let square_sum = samples
        .iter()
        .map(|sample| {
            let sample = f64::from(*sample);
            sample * sample
        })
        .sum::<f64>();
    (square_sum / samples.len() as f64).sqrt() as f32
}

#[derive(Default)]
struct RollingHypothesis {
    previous: Vec<String>,
    committed_words: usize,
}

struct HypothesisUpdate {
    finals: Vec<String>,
    partial: String,
}

impl RollingHypothesis {
    fn observe(&mut self, text: &str) -> HypothesisUpdate {
        let words = words(text);
        let common = self
            .previous
            .iter()
            .zip(&words)
            .take_while(|(left, right)| left == right)
            .count();
        let stable = common.saturating_sub(1).max(self.committed_words);
        let finals = if stable > self.committed_words && stable <= words.len() {
            vec![words[self.committed_words..stable].join(" ")]
        } else {
            Vec::new()
        };
        self.committed_words = stable.min(words.len());
        let partial = words[self.committed_words..].join(" ");
        self.previous = words;
        HypothesisUpdate { finals, partial }
    }

    fn finish(&mut self, text: &str) -> Vec<String> {
        let words = words(text);
        let start = self.committed_words.min(words.len());
        let remainder = words[start..].join(" ");
        if remainder.is_empty() {
            Vec::new()
        } else {
            vec![remainder]
        }
    }
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

struct MlxWorker {
    commands: std_mpsc::SyncSender<InferenceRequest>,
    results: mpsc::UnboundedReceiver<InferenceResult>,
    task: Option<thread::JoinHandle<()>>,
}

struct InferenceRequest {
    source: MeetingSource,
    generation: u64,
    samples: Vec<i16>,
    final_batch: bool,
}

struct InferenceResult {
    source: MeetingSource,
    generation: u64,
    text: String,
    final_batch: bool,
    error: Option<String>,
}

impl MlxWorker {
    async fn spawn(model_id: Arc<str>) -> Result<Self, MeetingFailure> {
        let (commands, receiver) = std_mpsc::sync_channel(COMMAND_CAPACITY);
        let (results, result_receiver) = mpsc::unbounded_channel();
        let (ready, initialized) = oneshot::channel();
        let task = thread::Builder::new()
            .name("nanocodex-meeting-mlx".to_owned())
            .spawn(move || run_worker(&model_id, receiver, &results, ready))
            .map_err(|error| {
                MeetingFailure::Mlx(format!("failed to spawn inference worker: {error}"))
            })?;
        initialized
            .await
            .map_err(|_| MeetingFailure::Mlx("inference worker stopped during startup".to_owned()))?
            .map_err(MeetingFailure::Mlx)?;
        Ok(Self {
            commands,
            results: result_receiver,
            task: Some(task),
        })
    }

    fn shutdown(mut self) -> Result<(), MeetingFailure> {
        drop(self.commands);
        if let Some(task) = self.task.take() {
            task.join()
                .map_err(|_| MeetingFailure::Mlx("inference worker panicked".to_owned()))?;
        }
        Ok(())
    }
}

fn run_worker(
    model_id: &str,
    commands: std_mpsc::Receiver<InferenceRequest>,
    results: &mpsc::UnboundedSender<InferenceResult>,
    ready: oneshot::Sender<Result<(), String>>,
) {
    let assets = match stage_assets() {
        Ok(assets) => assets,
        Err(error) => {
            drop(ready.send(Err(error)));
            return;
        }
    };
    let mut model = match load_model(model_id) {
        Ok(model) => model,
        Err(error) => {
            drop(ready.send(Err(format!("failed to load `{model_id}`: {error}"))));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    let options = TranscribeOptions {
        condition_on_previous_text: false,
        temperatures: vec![0.0],
        verbose: false,
        ..TranscribeOptions::default()
    };
    while let Ok(request) = commands.recv() {
        let mut pcm = Vec::with_capacity(request.samples.len() * size_of::<i16>());
        for sample in &request.samples {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let audio = audio_from_pcm_s16le(&pcm);
        let result = transcribe(audio, &mut model, assets.path(), &options);
        let (text, error) = match result {
            Ok(transcript) => (transcript.text, None),
            Err(error) => (String::new(), Some(error.to_string())),
        };
        if results
            .send(InferenceResult {
                source: request.source,
                generation: request.generation,
                text,
                final_batch: request.final_batch,
                error,
            })
            .is_err()
        {
            break;
        }
    }
}

fn stage_assets() -> Result<tempfile::TempDir, String> {
    let directory = tempfile::Builder::new()
        .prefix("nanocodex-whisper-assets-")
        .tempdir()
        .map_err(|error| format!("failed to stage Whisper assets: {error}"))?;
    for (name, contents) in [
        ("multilingual.tiktoken", MULTILINGUAL_TOKENIZER),
        ("mel_filters_80.npy", MEL_FILTERS_80),
        ("mel_filters_128.npy", MEL_FILTERS_128),
    ] {
        std::fs::write(directory.path().join(name), contents)
            .map_err(|error| format!("failed to stage `{name}`: {error}"))?;
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::{
        InferenceResult, MeetingEvent, MeetingSource, RollingHypothesis, SourceState,
        apply_inference_result, rms,
    };

    #[test]
    fn rolling_hypotheses_promote_only_a_repeated_word_prefix() {
        let mut hypothesis = RollingHypothesis::default();
        let first = hypothesis.observe("we should ship on Thursday");
        assert!(first.finals.is_empty());
        assert_eq!(first.partial, "we should ship on Thursday");

        let second = hypothesis.observe("we should ship on Thursday morning");
        assert_eq!(second.finals, ["we should ship on"]);
        assert_eq!(second.partial, "Thursday morning");

        assert_eq!(
            hypothesis.finish("we should ship on Thursday morning"),
            ["Thursday morning"]
        );
    }

    #[test]
    fn energy_gate_distinguishes_silence_from_speech() {
        assert_eq!(rms(&[0; 480]), 0.0);
        assert!(rms(&[1_000; 480]) > 900.0);
    }

    #[test]
    fn a_final_batch_always_clears_the_unstable_hypothesis() {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut microphone = SourceState::new();
        let mut system = SourceState::new();

        apply_inference_result(
            InferenceResult {
                source: MeetingSource::Microphone,
                generation: 0,
                text: String::new(),
                final_batch: true,
                error: None,
            },
            &events,
            &mut microphone,
            &mut system,
        )
        .unwrap();

        assert!(matches!(
            receiver.try_recv().unwrap(),
            MeetingEvent::TranscriptPartial {
                source: MeetingSource::Microphone,
                text,
            } if text.is_empty()
        ));
    }
}
