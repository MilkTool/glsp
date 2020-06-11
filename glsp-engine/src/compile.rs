#![cfg(feature = "compiler")]

use flate2::{Compression, write::{DeflateDecoder, DeflateEncoder}};
use fnv::{FnvHashMap};
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::Entry::{Occupied, Vacant}, VecDeque};
use std::convert::{TryInto};
use std::io::{Write};
use std::iter::{repeat_with};
use super::code::{Bytecode, Instr, Lambda, ParamMap, Stay, StaySource};
use super::engine::{Filename, glsp, Span, SpanStorage, Sym};
use super::error::{GResult};
use super::gc::{GcHeader, Slot, Root};
use super::val::{Val};

/*
this module is only present when the "compiler" crate feature is enabled.

it provides
	- a Recording struct which caches the compiled toplevel forms produced by each (load) 
	  operation, and which can be serialized into a stream of bytes (with proper deduplication of 
	  things like Filenames and Spans)
	- a fn Recording::from_bytes which consumes one of those byte streams, allocating and returning
	  a Recording which is semantically identical to the original input. evaler.rs will access that
	  data when (load)ing each file, executing the Recording rather than accessing the file
	  itself, and validating that the sequence of (load) operations is identical to those recorded
*/

//-------------------------------------------------------------------------------------------------
// Recording
//-------------------------------------------------------------------------------------------------

/*
the main source of complexity in the recorder comes from needing to deduplicate Spans
and Filenames. if we were to store each Span as the tree of SpanStorages which it represents, that
would massively inflate the size of a serialized Chunk. on the other hand, we don't want to
serialize the full SpanStorage database into every Chunk...

instead, we serialize only those SpanStorages which are referred to by the Chunk. in the process, 
we convert Spans from their original sparse representation into a dense range of indexes (say, 
every index from 0 to 350), with indexes being represented by DenseSpans. when deserializing,
we register each SpanStorage back into the spans database, then it's a cheap array lookup to
convert each DenseSpan back into a normal Span.

we perform a similar transformation to convert Filenames into DenseFilenames, and to preserve
the identity of Gc<Stay>s (rather than serializing each Gc<Stay> as though it has exclusive 
ownership of a distinct allocation).

Bytecodes are not actually mutable, so Chunks store DenseBytecode and DenseLambda types
which are convertible to/from Bytecode and Lambda.
*/

pub(crate) struct Recording {
	actions: VecDeque<Action>
}

//we encode the recording as a linear sequence of Actions, most of which are just a toplevel
//Bytecode to be executed. one complication is (load) calls - they need to be fully executed
//partway through the execution of another Bytecode - but we just leave that incomplete Bytecode 
//on the Rust call-stack while performing the (load), then return to it afterwards.

pub(crate) enum Action {
	//execute this Bytecode
	Execute(Root<Bytecode>),

	//assign the next Execute result to the given Stay
	ToplevelLet(Root<Stay>),

	//start or finish loading the given file
	StartLoad(Filename),
	EndLoad
}

//we don't perform deflate compression for small inputs (e.g. those generated by an eval!() macro), 
//since the decompression is surprisingly expensive: about 80us for 122 compressed bytes!
const DEFLATE_LIMIT: usize = 8 * 1024;

impl Recording {
	pub(crate) fn new() -> Recording {
		Recording {
			actions: VecDeque::new()
		}
	}

	pub(crate) fn is_empty(&self) -> bool {
		self.actions.is_empty()
	}

	pub(crate) fn peek(&self) -> GResult<&Action> {
		match self.actions.front() {
			Some(action) => Ok(action),
			None => bail!("unexpected end of compiled actions")
		}
	}

	pub(crate) fn pop(&mut self) -> GResult<Action> {
		match self.actions.pop_front() {
			Some(action) => Ok(action),
			None => bail!("unexpected end of compiled actions")
		}
	}

	pub(crate) fn add_action(&mut self, action: Action) {
		self.actions.push_back(action)
	}

	pub(crate) fn into_bytes(self) -> Vec<u8> {
		let mut conv = DenseConverter::default();

		let actions = self.actions.iter().map(|action| {
			DenseAction::from_action(action, &mut conv)
		}).collect();

		let chunk = Chunk {
			actions,
			span_storage: conv.span_storage,
			filename_storage: conv.filename_storage,
			stay_count: conv.stay_map.len()
		};

		//we use `bincode` because `serde_cbor` produces a larger output (even when using
		//`to_packed_vec` followed by deflate compression) which is also slower to read back in. 
		let raw_bytes = bincode::serialize(&chunk).unwrap();

		//we store a u64 uncompressed length, followed by the deflated payload. using
		//Compression::default rather than Compression::best only increases the payload size
		//by 3%, and it doubles the compression speed.
		let mut compressed = Vec::<u8>::new();
		compressed.extend_from_slice(&(raw_bytes.len() as u64).to_le_bytes());

		if raw_bytes.len() < DEFLATE_LIMIT {
			compressed.extend_from_slice(&raw_bytes[..]);
		} else {
			let mut encoder = DeflateEncoder::new(&mut compressed, Compression::default());
			encoder.write_all(&raw_bytes[..]).unwrap();
		}

		compressed
	}

	pub(crate) fn from_bytes(bytes: &[u8]) -> GResult<Recording> {
		//decompress the payload
		ensure!(bytes.len() >= 8);
		let decompressed_len = u64::from_le_bytes((&bytes[..8]).try_into().unwrap());

		let mut inflate_storage: Option<Vec<u8>>;
		let decompressed = if decompressed_len < DEFLATE_LIMIT as u64 {
			&bytes[8..]
		} else {
			inflate_storage = Some(Vec::<u8>::with_capacity(decompressed_len as usize));

			let mut decoder = DeflateDecoder::new(inflate_storage.as_mut().unwrap());
			decoder.write_all(&bytes[8..]).unwrap();
			drop(decoder);

			&inflate_storage.as_ref().unwrap()[..]
		};

		//decode the decompressed bytes
		let chunk: Chunk = match bincode::deserialize(decompressed) {
			Ok(chunk) => chunk,
			Err(e) => return Err(error!("error when deserializing compiled bytes").with_source(e))
		};

		let mut conv = SparseConverter::new(&chunk);

		let actions = chunk.actions.into_iter().map(|dense_action| {
			dense_action.into_action(&mut conv)
		}).collect();

		Ok(Recording { actions })
	}
}

//the data which is actually serialized to/from a byte slice
#[derive(Deserialize, Serialize)]
struct Chunk {
	actions: Vec<DenseAction>,
	span_storage: Vec<DenseSpanStorage>,
	filename_storage: Vec<String>,

	//we don't need to store Stays' value, only the fact that they exist. the deserializer 
	//allocates them all up front, but sets them to #n. they'll be initialized dynamically when 
	//Action::ToplevelLet is evaluated
	stay_count: usize
}

//the state required to convert a Span into a DenseSpan, a Bytecode into a DenseBytecode, etc.
#[derive(Default)]
struct DenseConverter {
	span_map: FnvHashMap<Span, DenseSpan>,
	span_storage: Vec<DenseSpanStorage>,
	filename_map: FnvHashMap<Filename, DenseFilename>,
	filename_storage: Vec<String>,
	stay_map: FnvHashMap<*const Stay, DenseStay>
}

struct SparseConverter {
	spans: Vec<Span>,
	filenames: Vec<Filename>,
	stays: Vec<Root<Stay>>
}

impl SparseConverter {
	fn new(chunk: &Chunk) -> SparseConverter {
		let mut conv = SparseConverter {
			spans: Vec::with_capacity(chunk.span_storage.len()),
			filenames: chunk.filename_storage.iter().map(|st| glsp::filename(st)).collect(),
			stays: repeat_with(|| {
				glsp::alloc(Stay::new(Slot::Nil))
			}).take(chunk.stay_count).collect()
		};

		//this works because Expanded spans only refer to spans which already exist, i.e., spans
		//with an earlier index than themselves
		for dense_storage in &chunk.span_storage {
			let span = glsp::span(dense_storage.to_span_storage(&mut conv));
			conv.spans.push(span);
		}

		conv
	}
}

#[derive(Deserialize, Serialize)]
enum DenseAction {
	Execute(Box<DenseBytecode>),
	ToplevelLet(DenseStay),
	StartLoad(DenseFilename),
	EndLoad
}

impl DenseAction {
	fn from_action(action: &Action, conv: &mut DenseConverter) -> DenseAction {
		match *action {
			Action::Execute(ref bytecode) => {
				let dense_bytecode = DenseBytecode::from_bytecode(bytecode, conv);
				DenseAction::Execute(Box::new(dense_bytecode))
			}
			Action::ToplevelLet(ref stay) => {
				DenseAction::ToplevelLet(DenseStay::from_stay(stay, conv))
			}
			Action::StartLoad(filename) => {
				DenseAction::StartLoad(DenseFilename::from_filename(filename, conv))
			}
			Action::EndLoad => {
				DenseAction::EndLoad
			}
		}
	}

	fn into_action(self, conv: &mut SparseConverter) -> Action {
		match self {
			DenseAction::Execute(dense_bytecode) => {
				Action::Execute(dense_bytecode.into_bytecode(conv))
			}
			DenseAction::ToplevelLet(dense_stay) => {
				Action::ToplevelLet(dense_stay.to_stay(conv))
			}
			DenseAction::StartLoad(dense_filename) => {
				Action::StartLoad(dense_filename.to_filename(conv))
			}
			DenseAction::EndLoad => {
				Action::EndLoad
			}
		}
	}
}

#[derive(Copy, Clone, Deserialize, Serialize)]
struct DenseSpan(u32);

impl DenseSpan {
	fn from_span(span: Span, conv: &mut DenseConverter) -> DenseSpan {
		match conv.span_map.get(&span) {
			Some(dense_span) => *dense_span,
			None => {
				let dense_storage = DenseSpanStorage::from_span_storage(
					glsp::span_storage(span), conv
				);
				conv.span_storage.push(dense_storage);

				let dense_span = DenseSpan((conv.span_storage.len() - 1) as u32);
				conv.span_map.insert(span, dense_span);

				dense_span
			}
		}
	}

	fn to_span(&self, conv: &mut SparseConverter) -> Span {
		conv.spans[self.0 as usize]
	}
}

#[derive(Copy, Clone, Deserialize, Serialize)]
enum DenseSpanStorage {
	Loaded(DenseFilename, usize),
	Expanded(Option<Sym>, DenseSpan, DenseSpan),
	Generated
}

impl DenseSpanStorage {
	fn from_span_storage(
		span_storage: SpanStorage,
		conv: &mut DenseConverter
	) -> DenseSpanStorage {

		match span_storage {
			SpanStorage::Loaded(filename, line) => {
				let dense_filename = DenseFilename::from_filename(filename, conv);
				DenseSpanStorage::Loaded(dense_filename, line)
			}
			SpanStorage::Expanded(sym, span0, span1) => {
				let dense_span0 = DenseSpan::from_span(span0, conv);
				let dense_span1 = DenseSpan::from_span(span1, conv);
				DenseSpanStorage::Expanded(sym, dense_span0, dense_span1)
			}
			SpanStorage::Generated => {
				DenseSpanStorage::Generated
			}
		}
	}

	fn to_span_storage(&self, conv: &mut SparseConverter) -> SpanStorage {
		match *self {
			DenseSpanStorage::Loaded(dense_filename, line) => {
				let filename = dense_filename.to_filename(conv);
				SpanStorage::Loaded(filename, line)
			}
			DenseSpanStorage::Expanded(sym, dense_span0, dense_span1) => {
				let span0 = dense_span0.to_span(conv);
				let span1 = dense_span1.to_span(conv);
				SpanStorage::Expanded(sym, span0, span1)
			}
			DenseSpanStorage::Generated => {
				SpanStorage::Generated
			}
		}
	}
}

#[derive(Copy, Clone, Deserialize, Serialize)]
struct DenseFilename(u32);

impl DenseFilename {
	fn from_filename(filename: Filename, conv: &mut DenseConverter) -> DenseFilename {
		match conv.filename_map.entry(filename) {
			Occupied(entry) => *entry.get(),
			Vacant(entry) => {
				conv.filename_storage.push(glsp::filename_str(filename).to_string());
				let dense_filename = DenseFilename((conv.filename_storage.len() - 1) as u32);
				entry.insert(dense_filename);
				dense_filename
			}
		}
	}

	fn to_filename(&self, conv: &mut SparseConverter) -> Filename {
		conv.filenames[self.0 as usize]
	}
}

#[derive(Copy, Clone, Deserialize, Serialize)]
struct DenseStay(u32);

impl DenseStay {
	fn from_stay(stay: &Root<Stay>, conv: &mut DenseConverter) -> DenseStay {
		let stay_map_len = conv.stay_map.len();
		match conv.stay_map.entry(&**stay as *const Stay) {
			Occupied(entry) => *entry.get(),
			Vacant(entry) => {
				let dense_stay = DenseStay(stay_map_len as u32);
				entry.insert(dense_stay);
				dense_stay
			}
		}
	}

	fn to_stay(&self, conv: &mut SparseConverter) -> Root<Stay> {
		conv.stays[self.0 as usize].clone()
	}
}

#[derive(Copy, Clone, Deserialize, Serialize)]
enum DenseStaySource {
	Empty,
	Param(u8),
	Captured(u8),
	PreExisting(DenseStay)
}

impl DenseStaySource {
	fn from_stay_source(stay_source: &StaySource, conv: &mut DenseConverter) -> DenseStaySource {
		match stay_source {
			StaySource::Empty => DenseStaySource::Empty,
			StaySource::Param(id) => DenseStaySource::Param(*id),
			StaySource::Captured(id) => DenseStaySource::Captured(*id),
			StaySource::PreExisting(stay) => {
				DenseStaySource::PreExisting(DenseStay::from_stay(&stay.root(), conv))
			}
		}
	}

	fn to_stay_source(&self, conv: &mut SparseConverter) -> StaySource {
		match self {
			DenseStaySource::Empty => StaySource::Empty,
			DenseStaySource::Param(id) => StaySource::Param(*id),
			DenseStaySource::Captured(id) => StaySource::Captured(*id),
			DenseStaySource::PreExisting(dense_stay) => {
				StaySource::PreExisting(dense_stay.to_stay(conv).into_gc())
			}
		}
	}
}

#[derive(Deserialize, Serialize)]
struct DenseBytecode {
	instrs: Vec<Instr>,
	spans: Vec<DenseSpan>,
	start_regs: Vec<Val>,
	start_stays: Vec<DenseStaySource>,
	local_count: u8,
	scratch_count: u8,
	literal_count: u8,
	lambdas: Vec<Box<DenseLambda>>,
	defers: Vec<usize>
}

impl DenseBytecode {
	fn from_bytecode(src: &Bytecode, conv: &mut DenseConverter) -> DenseBytecode {
		DenseBytecode {
			instrs: src.instrs.clone(),
			spans: src.spans.iter().map(|span| DenseSpan::from_span(*span, conv)).collect(),
			start_regs: src.start_regs.iter().map(Slot::root).collect(),
			start_stays: src.start_stays.iter().map(|stay_source| {
				DenseStaySource::from_stay_source(stay_source, conv)
			}).collect(),
			local_count: src.local_count,
			scratch_count: src.scratch_count,
			literal_count: src.literal_count,
			lambdas: src.lambdas.iter().map(|lambda| {
				Box::new(DenseLambda::from_lambda(lambda, conv))
			}).collect(),
			defers: src.defers.clone()
		}
	}

	fn into_bytecode(self, conv: &mut SparseConverter) -> Root<Bytecode> {
		let DenseBytecode { 
			instrs,
			spans, 
			start_regs, 
			start_stays,
			local_count,
			scratch_count,
			literal_count,
			lambdas,
			defers
		} = self;

		glsp::alloc(Bytecode {
			header: GcHeader::new(),
			instrs,
			spans: spans.iter().map(|span| span.to_span(conv)).collect(),
			start_regs: start_regs.iter().map(Slot::from_val).collect(),
			start_stays: start_stays.iter().map(|source| source.to_stay_source(conv)).collect(),
			local_count,
			scratch_count,
			literal_count,
			lambdas: lambdas.into_iter().map(|dense_lambda| {
				dense_lambda.into_lambda(conv).into_gc()
			}).collect(),
			defers
		})
	}
}

#[derive(Deserialize, Serialize)]
struct DenseLambda {
	bytecode: Box<DenseBytecode>,
	param_map: ParamMap,
	name: Option<Sym>,
	captures: Vec<u8>,
	yields: bool
}

impl DenseLambda {
	fn from_lambda(src: &Lambda, conv: &mut DenseConverter) -> DenseLambda {
		DenseLambda {
			bytecode: Box::new(DenseBytecode::from_bytecode(&src.bytecode, conv)),
			param_map: src.param_map.clone(),
			name: src.name.clone(),
			captures: src.captures.clone(),
			yields: src.yields
		}
	}

	fn into_lambda(self, conv: &mut SparseConverter) -> Root<Lambda> {
		let DenseLambda {
			bytecode: dense_bytecode,
			param_map,
			name,
			captures,
			yields
		} = self;

		glsp::alloc(Lambda {
			header: GcHeader::new(),

			bytecode: dense_bytecode.into_bytecode(conv).into_gc(),
			param_map,
			name,
			captures,
			yields
		})
	}
}