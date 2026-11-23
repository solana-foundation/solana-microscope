use std::sync::{Arc, Mutex};

use carbon_core::{filter::Filter, pipeline::PipelineBuilder};
use serde::Serialize;
use serde_json::Value;
use solana_pubkey::Pubkey;

use crate::processor::{EventProcessor, RecentTransactions};

/// One decoder generated from a configured IDL. `microscope_programs` lists
/// them through `for_each_program!`, in the order `microscope.toml` does.
pub trait Program: Send + Sync + 'static {
    const DECLARES_EVENTS: bool;
    type Instruction: Serialize + Send + Sync + 'static;

    fn is_event_wrapper(instruction: &Self::Instruction) -> bool;

    /// `None` when the data is not one of this program's events.
    fn decode_event(data: &[u8]) -> Option<serde_json::Result<Value>>;
}

macro_rules! implement {
    ($module:ident, events) => {
        impl Program for microscope_programs::$module::Decoder {
            const DECLARES_EVENTS: bool = true;
            type Instruction = microscope_programs::$module::Instruction;

            fn is_event_wrapper(instruction: &Self::Instruction) -> bool {
                matches!(
                    instruction,
                    microscope_programs::$module::Instruction::CpiEvent { .. }
                )
            }

            fn decode_event(data: &[u8]) -> Option<serde_json::Result<Value>> {
                microscope_programs::$module::CpiEvent::decode(data)
                    .map(|event| serde_json::to_value(&event))
            }
        }
    };
    ($module:ident, no_events) => {
        impl Program for microscope_programs::$module::Decoder {
            const DECLARES_EVENTS: bool = false;
            type Instruction = microscope_programs::$module::Instruction;

            fn is_event_wrapper(_: &Self::Instruction) -> bool {
                false
            }

            fn decode_event(_: &[u8]) -> Option<serde_json::Result<Value>> {
                None
            }
        }
    };
}
microscope_programs::for_each_program!(implement);

pub struct CompiledProgram {
    pub id: Pubkey,
    pub idl_sha256: &'static str,
    pub declares_events: bool,
}

pub fn compiled() -> Vec<CompiledProgram> {
    let mut programs = Vec::new();
    macro_rules! push {
        ($module:ident, $events:ident) => {
            programs.push(CompiledProgram {
                id: microscope_programs::$module::PROGRAM_ID,
                idl_sha256: microscope_programs::$module::IDL_SHA256,
                declares_events:
                    <microscope_programs::$module::Decoder as Program>::DECLARES_EVENTS,
            });
        };
    }
    microscope_programs::for_each_program!(push);
    programs
}

pub fn with_pipes(
    mut builder: PipelineBuilder,
    filters: fn() -> Vec<Box<dyn Filter>>,
) -> PipelineBuilder {
    let recent_transactions = Arc::new(Mutex::new(RecentTransactions::default()));
    macro_rules! register {
        ($module:ident, $events:ident) => {
            builder = builder.instruction_with_filters(
                microscope_programs::$module::Decoder,
                EventProcessor::<microscope_programs::$module::Decoder>::new(
                    recent_transactions.clone(),
                ),
                filters(),
            );
        };
    }
    microscope_programs::for_each_program!(register);
    builder
}
