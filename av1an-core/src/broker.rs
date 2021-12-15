use crate::{
  ffmpeg, finish_multi_progress_bar, finish_progress_bar, get_done,
  progress_bar::{dec_bar, dec_mp_bar},
  settings::EncodeArgs,
  Chunk, Encoder, Instant, TargetQuality, Verbosity,
};
use std::{
  fmt::{Debug, Display},
  fs::File,
  io::Write,
  path::Path,
  process::ExitStatus,
  sync::mpsc::Sender,
};

use cfg_if::cfg_if;
use memchr::memmem;
use smallvec::SmallVec;
use thiserror::Error;

pub struct Broker<'a> {
  pub max_tries: usize,
  pub chunk_queue: Vec<Chunk>,
  pub project: &'a EncodeArgs,
  pub target_quality: Option<TargetQuality<'a>>,
}

#[derive(Clone)]
pub enum StringOrBytes {
  String(String),
  Bytes(Vec<u8>),
}

impl Debug for StringOrBytes {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::String(s) => {
        if f.alternate() {
          f.write_str(&textwrap::indent(s, /* 8 spaces */ "        "))?;
        } else {
          f.write_str(s)?;
        }
      }
      Self::Bytes(b) => write!(f, "raw bytes: {:?}", b)?,
    }

    Ok(())
  }
}

impl From<Vec<u8>> for StringOrBytes {
  fn from(bytes: Vec<u8>) -> Self {
    if simdutf8::basic::from_utf8(&bytes).is_ok() {
      // SAFETY: this branch guarantees that the input is valid UTF8
      Self::String(unsafe { String::from_utf8_unchecked(bytes) })
    } else {
      Self::Bytes(bytes)
    }
  }
}

impl From<String> for StringOrBytes {
  fn from(s: String) -> Self {
    Self::String(s)
  }
}

impl StringOrBytes {
  pub fn as_bytes(&self) -> &[u8] {
    match self {
      Self::String(s) => s.as_bytes(),
      Self::Bytes(b) => &b,
    }
  }
}

#[derive(Error, Debug)]
pub struct EncoderCrash {
  pub exit_status: ExitStatus,
  pub stdout: StringOrBytes,
  pub stderr: StringOrBytes,
  pub pipe_stderr: StringOrBytes,
}

impl Display for EncoderCrash {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "encoder crashed: {}\nstdout:\n{:#?}\nstderr:\n{:#?}\nsource pipe stderr:\n{:#?}",
      self.exit_status, self.stdout, self.stderr, self.pipe_stderr
    )?;
    Ok(())
  }
}

impl<'a> Broker<'a> {
  /// Main encoding loop. set_thread_affinity may be ignored if the value is invalid.
  #[allow(clippy::needless_pass_by_value)]
  pub fn encoding_loop(self, tx: Sender<()>, mut set_thread_affinity: Option<usize>) {
    if !self.chunk_queue.is_empty() {
      let (sender, receiver) = crossbeam_channel::bounded(self.chunk_queue.len());

      for chunk in &self.chunk_queue {
        sender.send(chunk.clone()).unwrap();
      }
      drop(sender);

      cfg_if! {
        if #[cfg(any(target_os = "linux", target_os = "windows"))] {
          if let Some(threads) = set_thread_affinity {
            let available_threads = num_cpus::get();
            let requested_threads = threads.saturating_mul(self.project.workers);
            if requested_threads > available_threads {
              warn!(
                "ignoring set_thread_affinity: requested more threads than available ({}/{})",
                requested_threads, available_threads
              );
              set_thread_affinity = None;
            } else if requested_threads == 0 {
              warn!("ignoring set_thread_affinity: requested 0 threads");

              set_thread_affinity = None;
            }
          }
        }
      }

      crossbeam_utils::thread::scope(|s| {
        let consumers: Vec<_> = (0..self.project.workers)
          .map(|idx| (receiver.clone(), &self, idx))
          .map(|(rx, queue, worker_id)| {
            let tx = tx.clone();
            s.spawn(move |_| {
              cfg_if! {
                if #[cfg(any(target_os = "linux", target_os = "windows"))] {
                  if let Some(threads) = set_thread_affinity {
                    let mut cpu_set = SmallVec::<[usize; 16]>::new();
                    cpu_set.extend((threads * worker_id..).take(threads));
                    if let Err(e) = affinity::set_thread_affinity(&cpu_set) {
                      warn!(
                        "failed to set thread affinity for worker {}: {}",
                        worker_id, e
                      );
                    }
                  }
                }
              }

              while let Ok(mut chunk) = rx.recv() {
                if let Err(e) = queue.encode_chunk(&mut chunk, worker_id) {
                  error!("[chunk {}] {}", chunk.index, e);

                  tx.send(()).unwrap();
                  return Err(());
                }
              }
              Ok(())
            })
          })
          .collect();
        for consumer in consumers {
          let _ = consumer.join().unwrap();
        }
      })
      .unwrap();

      if self.project.verbosity == Verbosity::Normal {
        finish_progress_bar();
      } else if self.project.verbosity == Verbosity::Verbose {
        finish_multi_progress_bar();
      }
    }
  }

  fn encode_chunk(&self, chunk: &mut Chunk, worker_id: usize) -> Result<(), EncoderCrash> {
    let st_time = Instant::now();

    // space padding at the beginning to align with "finished chunk"
    debug!(" started chunk {:05}: {} frames", chunk.index, chunk.frames);

    if let Some(ref tq) = self.target_quality {
      tq.per_shot_target_quality_routine(chunk)?;
    }

    // Run all passes for this chunk
    let mut tpl_crash_workaround = false;
    for current_pass in 1..=self.project.passes {
      for r#try in 1..=self.max_tries {
        let res = self
          .project
          .create_pipes(chunk, current_pass, worker_id, tpl_crash_workaround);
        if let Err((e, frames)) = res {
          if self.project.verbosity == Verbosity::Normal {
            dec_bar(frames);
          } else if self.project.verbosity == Verbosity::Verbose {
            dec_mp_bar(frames);
          }

          if r#try == self.max_tries {
            error!(
              "[chunk {}] encoder crashed {} times, shutting down worker",
              chunk.index, self.max_tries
            );
            return Err(e);
          }
          // avoids double-print of the error message as both a WARN and ERROR,
          // since `Broker::encoding_loop` will print the error message as well
          warn!("Encoder failed (on chunk {}):\n{}", chunk.index, e);

          if self.project.encoder == Encoder::aom
            && !tpl_crash_workaround
            && memmem::rfind(e.stderr.as_bytes(), b"av1_tpl_stats_ready").is_some()
          {
            // aomenc has had a history of crashes related to TPL on certain chunks,
            // particularly in videos with less motion, such as animated content.
            // This workaround retries a chunk with TPL disabled if such a crash is detected.
            // Although there is some amount of psychovisual quality loss with TPL disabled,
            // this is preferable to being unable to complete the encode.
            warn!("TPL-based crash, retrying chunk without TPL");
            tpl_crash_workaround = true;
          }
        } else {
          break;
        }
      }
    }

    let encoded_frames = ffmpeg::num_frames(chunk.output().as_ref()).unwrap();

    if encoded_frames == chunk.frames {
      let enc_time = st_time.elapsed();
      let fps = encoded_frames as f64 / enc_time.as_secs_f64();

      let progress_file = Path::new(&self.project.temp).join("done.json");
      get_done().done.insert(chunk.name(), encoded_frames);

      let mut progress_file = File::create(&progress_file).unwrap();
      progress_file
        .write_all(serde_json::to_string(get_done()).unwrap().as_bytes())
        .unwrap();

      debug!(
        "finished chunk {:05}: {} frames, {:.2} fps, took {:.2?}",
        chunk.index, chunk.frames, fps, enc_time
      );
    } else {
      warn!(
        "finished chunk: FRAME MISMATCH: chunk {}: {}/{} (actual/expected frames)",
        chunk.index, encoded_frames, chunk.frames
      );
    }

    Ok(())
  }
}
