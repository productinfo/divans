// Copyright 2017 Dropbox, Inc
//
//   Licensed under the Apache License, Version 2.0 (the "License");
//   you may not use this file except in compliance with the License.
//   You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
//   Unless required by applicable law or agreed to in writing, software
//   distributed under the License is distributed on an "AS IS" BASIS,
//   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//   See the License for the specific language governing permissions and
//   limitations under the License.

use core::marker::PhantomData;
use core::cmp::{min, max};
use super::probability::{CDF2,CDF16};
use super::brotli;
pub use super::alloc::{AllocatedStackMemory, Allocator, SliceWrapper, SliceWrapperMut, StackAllocator};
pub use super::interface::{BlockSwitch, LiteralBlockSwitch, Command, Compressor, CopyCommand, Decompressor, DictCommand, LiteralCommand, Nop, NewWithAllocator, ArithmeticEncoderOrDecoder, LiteralPredictionModeNibble, PredictionModeContextMap, free_cmd, FeatureFlagSliceType};

pub use super::cmd_to_divans::EncoderSpecialization;
pub use codec::{EncoderOrDecoderSpecialization, DivansCodec, StrideSelection};
use super::resizable_buffer::ResizableByteBuffer;
use super::interface;
use super::brotli::BrotliResult;
use super::brotli::enc::encode::{BrotliEncoderStateStruct, BrotliEncoderCompressStream, BrotliEncoderOperation, BrotliEncoderIsFinished};
use super::divans_compressor::write_header;
pub struct BrotliDivansHybridCompressor<SelectedCDF:CDF16,
                            ChosenEncoder: ArithmeticEncoderOrDecoder + NewWithAllocator<AllocU8>,
                            AllocU8:Allocator<u8>,
                            AllocU16:Allocator<u16>,
                            AllocU32:Allocator<u32>,
                            AllocI32:Allocator<i32>,
                            AllocU64:Allocator<u64>,
                            AllocCommand:Allocator<super::brotli::enc::command::Command>,
                            AllocCDF2:Allocator<CDF2>,
                            AllocCDF16:Allocator<SelectedCDF>,
                            AllocF64: Allocator<brotli::enc::util::floatX>,
                            AllocFV: Allocator<brotli::enc::vectorization::Mem256f>,
                            AllocHL: Allocator<brotli::enc::histogram::HistogramLiteral>,
                            AllocHC: Allocator<brotli::enc::histogram::HistogramCommand>,
                            AllocHD: Allocator<brotli::enc::histogram::HistogramDistance>,
                            AllocHP: Allocator<brotli::enc::cluster::HistogramPair>,
                            AllocCT: Allocator<brotli::enc::histogram::ContextType>,
                            AllocHT: Allocator<brotli::enc::entropy_encode::HuffmanTree>,
                            AllocZN: Allocator<brotli::enc::ZopfliNode>
     > {
    brotli_encoder: BrotliEncoderStateStruct<AllocU8, AllocU16, AllocU32, AllocI32, AllocCommand>,
    codec: DivansCodec<ChosenEncoder, EncoderSpecialization, SelectedCDF, AllocU8, AllocCDF2, AllocCDF16>,
    header_progress: usize,
    window_size: u8,
    m64: AllocU64,
    mf64: AllocF64,
    mfv: AllocFV,
    mhl: AllocHL,
    mhc: AllocHC,
    mhd: AllocHD,
    mhp: AllocHP,
    mct: AllocCT,
    mht: AllocHT,
    mzn: AllocZN,
    brotli_data: ResizableByteBuffer<u8, AllocU8>,
    divans_data: ResizableByteBuffer<u8, AllocU8>,
    encoded_byte_offset: usize,
}



impl<SelectedCDF:CDF16,
     ChosenEncoder: ArithmeticEncoderOrDecoder + NewWithAllocator<AllocU8>,
     AllocU8:Allocator<u8>,
     AllocU16:Allocator<u16>,
     AllocU32:Allocator<u32>,
     AllocI32:Allocator<i32>,
     AllocU64:Allocator<u64>,
     AllocCommand:Allocator<super::brotli::enc::command::Command>,
     AllocCDF2:Allocator<CDF2>,
     AllocCDF16:Allocator<SelectedCDF>,
     AllocF64: Allocator<brotli::enc::util::floatX>,
     AllocFV: Allocator<brotli::enc::vectorization::Mem256f>,
     AllocHL: Allocator<brotli::enc::histogram::HistogramLiteral>,
     AllocHC: Allocator<brotli::enc::histogram::HistogramCommand>,
     AllocHD: Allocator<brotli::enc::histogram::HistogramDistance>,
     AllocHP: Allocator<brotli::enc::cluster::HistogramPair>,
     AllocCT: Allocator<brotli::enc::histogram::ContextType>,
     AllocHT: Allocator<brotli::enc::entropy_encode::HuffmanTree>,
     AllocZN: Allocator<brotli::enc::ZopfliNode>
     > BrotliDivansHybridCompressor<SelectedCDF,
                                    ChosenEncoder,
                                    AllocU8,
                                    AllocU16,
                                    AllocU32,
                                    AllocI32,
                                    AllocU64,
                                    AllocCommand,
                                    AllocCDF2,
                                    AllocCDF16,
                                    AllocF64,
                                    AllocFV,
                                    AllocHL,
                                    AllocHC,
                                    AllocHD,
                                    AllocHP,
                                    AllocCT,
                                    AllocHT,
                                    AllocZN> {
    pub fn get_m8(&mut self) -> &mut AllocU8 {
       self.codec.get_m8().get_base_alloc()
    }
    fn divans_encode_commands<SliceType:SliceWrapper<u8>+Default>(cmd:&[brotli::interface::Command<SliceType>],
                                                          header_progress: &mut usize,
                                                          data:&mut ResizableByteBuffer<u8, AllocU8>,
                                                          codec: &mut DivansCodec<ChosenEncoder,
                                                                                  EncoderSpecialization,
                                                                                  SelectedCDF,
                                                                                  AllocU8,
                                                                                  AllocCDF2,
                                                                                  AllocCDF16>,
                                                          window_size: u8) {
        let mut cmd_offset = 0usize;
        loop {
            let ret: BrotliResult;
            let mut output_offset = 0usize;
            {
                let output = data.checkout_next_buffer(codec.get_m8().get_base_alloc(),
                                                           Some(interface::HEADER_LENGTH + 256));
                if *header_progress != interface::HEADER_LENGTH {
                    match write_header(header_progress, window_size, output, &mut output_offset) {
                        BrotliResult::ResultSuccess => {},
                        _ => panic!("Unexpected failure writing header"),
                    }
                }
                let mut unused: usize = 0;
                ret = codec.encode_or_decode(&[],
                                             &mut unused,
                                             output,
                                             &mut output_offset,
                                             cmd,
                                             &mut cmd_offset);
            }
            match ret {
                BrotliResult::ResultSuccess | BrotliResult::NeedsMoreInput => {
                    assert_eq!(cmd_offset, cmd.len());
                    data.commit_next_buffer(output_offset);
                    return;
                },
                BrotliResult::ResultFailure => panic!("Unexpected error code"),
                BrotliResult::NeedsMoreOutput => {
                    data.commit_next_buffer(output_offset);
                }
            }
        }
    }
    fn internal_encode_stream(&mut self,
                              op: BrotliEncoderOperation,
                              input:&[u8], input_offset: &mut usize,
                              is_end: bool) -> brotli::BrotliResult {
        let mut nothing : Option<usize> = None;
        {
            let divans_data_ref = &mut self.divans_data;
            let divans_codec_ref = &mut self.codec;
            let header_progress_ref = &mut self.header_progress;
            let window_size = self.window_size;
            let mut closure = |a:&[brotli::interface::Command<brotli::InputReference>]| if a.len() != 0 {
                Self::divans_encode_commands(a,
                                             header_progress_ref,
                                             divans_data_ref,
                                             divans_codec_ref,
                                             window_size);
            };
            {
                let mut available_in = input.len() - *input_offset;
                if available_in == 0 && BrotliEncoderIsFinished(&mut self.brotli_encoder) != 0 {
                    return BrotliResult::ResultSuccess;
                }
                let mut available_out;
                let mut brotli_out_offset = 0usize;
                {
                    let brotli_buffer = self.brotli_data.checkout_next_buffer(&mut self.brotli_encoder.m8, Some(256));
                    available_out = brotli_buffer.len();

                    if BrotliEncoderCompressStream(&mut self.brotli_encoder,
                                                   &mut self.m64,
                                                   &mut self.mf64,
                                                   &mut self.mfv,
                                                   &mut self.mhl,
                                                   &mut self.mhc,
                                                   &mut self.mhd,
                                                   &mut self.mhp,
                                                   &mut self.mct,
                                                   &mut self.mht,
                                                   &mut self.mzn,
                                                   op,
                                                   &mut available_in,
                                                   input,
                                                   input_offset,
                                                   &mut available_out,
                                                   brotli_buffer,
                                                   &mut brotli_out_offset,
                                                   &mut nothing,
                                                   &mut closure) <= 0 {
                        return BrotliResult::ResultFailure;
                    }
                }
                self.brotli_data.commit_next_buffer(brotli_out_offset);
                if available_out != 0 && available_in == 0 && BrotliEncoderIsFinished(&mut self.brotli_encoder) == 0 {
                    return BrotliResult::NeedsMoreInput;
                }
            }
        }
        if is_end && BrotliEncoderIsFinished(&mut self.brotli_encoder) == 0 {
            return BrotliResult::NeedsMoreOutput;
        }
        if is_end {
            loop { // flush divans coder
                let ret;
                let mut output_offset = 0usize;
                {
                    let mut output = self.divans_data.checkout_next_buffer(self.codec.get_m8().get_base_alloc(),
                                                                           Some(interface::HEADER_LENGTH + 256));
                    ret = self.codec.flush(&mut output, &mut output_offset);
                }
                self.divans_data.commit_next_buffer(output_offset);
                match ret {
                    BrotliResult::ResultSuccess => return ret,
                    BrotliResult::NeedsMoreOutput => {},
                    BrotliResult::NeedsMoreInput | BrotliResult::ResultFailure => return BrotliResult::ResultFailure,
                }
            }
        } else {
            return BrotliResult::NeedsMoreInput
        }
    }
    fn free_internal(&mut self) {
        self.brotli_data.free(&mut self.brotli_encoder.m8);
        self.divans_data.free(&mut self.codec.get_m8().get_base_alloc());
        brotli::enc::encode::BrotliEncoderDestroyInstance(&mut self.brotli_encoder);
    }
    pub fn free_ref(&mut self) {
        self.free_internal();
        self.codec.free_ref();
    }
    pub fn free(mut self) -> (AllocU8, AllocU32, AllocCDF2, AllocCDF16, AllocU8, AllocU16, AllocI32, AllocCommand,
                              AllocU64, AllocF64, AllocFV, AllocHL, AllocHC, AllocHD, AllocHP, AllocCT, AllocHT, AllocZN) {
        self.free_internal();
        let (m8, mcdf2, mcdf16) = self.codec.free();
        (m8, self.brotli_encoder.m32, mcdf2, mcdf16, self.brotli_encoder.m8, self.brotli_encoder.m16,self.brotli_encoder.mi32, self.brotli_encoder.mc,
         self.m64, self.mf64, self.mfv, self.mhl, self.mhc, self.mhd, self.mhp, self.mct, self.mht, self.mzn)
    }
}

impl<SelectedCDF:CDF16,
     ChosenEncoder: ArithmeticEncoderOrDecoder + NewWithAllocator<AllocU8>,
     AllocU8:Allocator<u8>,
     AllocU16:Allocator<u16>,
     AllocU32:Allocator<u32>,
     AllocI32:Allocator<i32>,
     AllocU64:Allocator<u64>,
     AllocCommand:Allocator<super::brotli::enc::command::Command>,
     AllocCDF2:Allocator<CDF2>,
     AllocCDF16:Allocator<SelectedCDF>,
     AllocF64: Allocator<brotli::enc::util::floatX>,
     AllocFV: Allocator<brotli::enc::vectorization::Mem256f>,
     AllocHL: Allocator<brotli::enc::histogram::HistogramLiteral>,
     AllocHC: Allocator<brotli::enc::histogram::HistogramCommand>,
     AllocHD: Allocator<brotli::enc::histogram::HistogramDistance>,
     AllocHP: Allocator<brotli::enc::cluster::HistogramPair>,
     AllocCT: Allocator<brotli::enc::histogram::ContextType>,
     AllocHT: Allocator<brotli::enc::entropy_encode::HuffmanTree>,
     AllocZN: Allocator<brotli::enc::ZopfliNode>
     > Compressor for BrotliDivansHybridCompressor<SelectedCDF,
                                                   ChosenEncoder,
                                                   AllocU8,
                                                   AllocU16,
                                                   AllocU32,
                                                   AllocI32,
                                                   AllocU64,
                                                   AllocCommand,
                                                   AllocCDF2,
                                                   AllocCDF16,
                                                   AllocF64,
                                                   AllocFV,
                                                   AllocHL,
                                                   AllocHC,
                                                   AllocHD,
                                                   AllocHP,
                                                   AllocCT,
                                                   AllocHT,
                                                   AllocZN> {
    fn encode(&mut self,
              input: &[u8],
              input_offset: &mut usize,
              _output: &mut [u8],
              _output_offset: &mut usize) -> BrotliResult {
        match self.internal_encode_stream(BrotliEncoderOperation::BROTLI_OPERATION_PROCESS,
                                          input,
                                          input_offset,
                                          false) {
            BrotliResult::ResultFailure => BrotliResult::ResultFailure,
            BrotliResult::ResultSuccess | BrotliResult::NeedsMoreInput => BrotliResult::NeedsMoreInput,
            BrotliResult::NeedsMoreOutput => panic!("unexpected code"),
        }
    }
    fn flush(&mut self,
             output: &mut [u8],
             output_offset: &mut usize) -> BrotliResult {
        let mut zero = 0usize;
        loop {
            match self.internal_encode_stream(BrotliEncoderOperation::BROTLI_OPERATION_FINISH,
                                              &[],
                                              &mut zero,
                                              true) {
                BrotliResult::ResultFailure => return BrotliResult::ResultFailure,
                BrotliResult::ResultSuccess => break,
                BrotliResult::NeedsMoreOutput => {},
                BrotliResult::NeedsMoreInput => panic!("unexpected code"),
            }
        }
        // we're in success area here
        let destination = output.split_at_mut(*output_offset).1;
        let src = self.divans_data.slice().split_at(self.encoded_byte_offset).1;
        let copy_len = min(src.len(), destination.len());
        destination.split_at_mut(copy_len).0.clone_from_slice(src.split_at(copy_len).0);
        *output_offset += copy_len;
        self.encoded_byte_offset += copy_len;
        if self.encoded_byte_offset == self.divans_data.len() {
            return BrotliResult::ResultSuccess;
        }
        BrotliResult::NeedsMoreOutput
    }
    fn encode_commands<SliceType:SliceWrapper<u8>+Default>(&mut self,
                                                           input:&[Command<SliceType>],
                                                           input_offset : &mut usize,
                                                           output :&mut[u8],
                                                           output_offset: &mut usize) -> BrotliResult {
        if self.header_progress != interface::HEADER_LENGTH {
            match write_header(&mut self.header_progress, self.window_size, output, output_offset) {
                BrotliResult::ResultSuccess => {},
                res => return res,
            }
        }
        let mut unused: usize = 0;
        self.codec.encode_or_decode(&[],
                                    &mut unused,
                                    output,
                                    output_offset,
                                    input,
                                    input_offset)
    }
}

pub struct BrotliDivansHybridCompressorFactory<AllocU8:Allocator<u8>,
     AllocU16:Allocator<u16>,
     AllocU32:Allocator<u32>,
     AllocI32:Allocator<i32>,
     AllocU64:Allocator<u64>,
     AllocCommand:Allocator<super::brotli::enc::command::Command>,
     AllocCDF2:Allocator<CDF2>,
     AllocCDF16:Allocator<interface::DefaultCDF16>,
     AllocF64: Allocator<brotli::enc::util::floatX>,
     AllocFV: Allocator<brotli::enc::vectorization::Mem256f>,
     AllocHL: Allocator<brotli::enc::histogram::HistogramLiteral>,
     AllocHC: Allocator<brotli::enc::histogram::HistogramCommand>,
     AllocHD: Allocator<brotli::enc::histogram::HistogramDistance>,
     AllocHP: Allocator<brotli::enc::cluster::HistogramPair>,
     AllocCT: Allocator<brotli::enc::histogram::ContextType>,
     AllocHT: Allocator<brotli::enc::entropy_encode::HuffmanTree>,
     AllocZN: Allocator<brotli::enc::ZopfliNode>> {
    p1: PhantomData<AllocU8>,
    p2: PhantomData<AllocCDF2>,
    p3: PhantomData<AllocCDF16>,
    p4: PhantomData<AllocU16>,
    p5: PhantomData<AllocU32>,
    p6: PhantomData<AllocI32>,
    p7: PhantomData<AllocCommand>,
    p8: PhantomData<AllocF64>,
    p9: PhantomData<AllocFV>,
    pa: PhantomData<AllocHL>,
    pb: PhantomData<AllocHC>,
    pc: PhantomData<AllocHD>,
    pd: PhantomData<AllocHP>,
    pe: PhantomData<AllocCT>,
    pf: PhantomData<AllocHT>,
    pg: PhantomData<AllocZN>,
    ph: PhantomData<AllocU64>,
}

impl<AllocU8:Allocator<u8>,
     AllocU16:Allocator<u16>,
     AllocI32:Allocator<i32>,
     AllocCommand:Allocator<super::brotli::enc::command::Command>,
     AllocU32:Allocator<u32>,
     AllocU64:Allocator<u64>,
     AllocCDF2:Allocator<CDF2>,
     AllocCDF16:Allocator<interface::DefaultCDF16>,
     AllocF64: Allocator<brotli::enc::util::floatX>,
     AllocFV: Allocator<brotli::enc::vectorization::Mem256f>,
     AllocHL: Allocator<brotli::enc::histogram::HistogramLiteral>,
     AllocHC: Allocator<brotli::enc::histogram::HistogramCommand>,
     AllocHD: Allocator<brotli::enc::histogram::HistogramDistance>,
     AllocHP: Allocator<brotli::enc::cluster::HistogramPair>,
     AllocCT: Allocator<brotli::enc::histogram::ContextType>,
     AllocHT: Allocator<brotli::enc::entropy_encode::HuffmanTree>,
     AllocZN: Allocator<brotli::enc::ZopfliNode>,
     > interface::DivansCompressorFactory<AllocU8, AllocU32, AllocCDF2, AllocCDF16>
    for BrotliDivansHybridCompressorFactory<AllocU8, AllocU16, AllocU32, AllocI32, AllocU64, AllocCommand, AllocCDF2, AllocCDF16,
                                            AllocF64, AllocFV, AllocHL, AllocHC, AllocHD, AllocHP, AllocCT, AllocHT, AllocZN> {
     type DefaultEncoder = DefaultEncoderType!();
     type ConstructedCompressor = BrotliDivansHybridCompressor<interface::DefaultCDF16,
                                                               Self::DefaultEncoder,
                                                               AllocU8,
                                                               AllocU16,
                                                               AllocU32,
                                                               AllocI32,
                                                               AllocU64,
                                                               AllocCommand,
                                                               AllocCDF2,
                                                               AllocCDF16,
                                                               AllocF64,
                                                               AllocFV,
                                                               AllocHL,
                                                               AllocHC,
                                                               AllocHD,
                                                               AllocHP,
                                                               AllocCT,
                                                               AllocHT,
                                                               AllocZN>;
      type AdditionalArgs = (AllocU8, AllocU16, AllocI32, AllocCommand,
                             AllocU64, AllocF64, AllocFV, AllocHL, AllocHC, AllocHD, AllocHP, AllocCT, AllocHT, AllocZN,
                             );
        fn new(mut m8: AllocU8, m32: AllocU32, mcdf2:AllocCDF2, mcdf16:AllocCDF16,
               opt: super::interface::DivansCompressorOptions,
               additional_args: Self::AdditionalArgs) -> Self::ConstructedCompressor {
        let window_size = min(24, max(10, opt.window_size.unwrap_or(22)));
        let enc = Self::DefaultEncoder::new(&mut m8);
        let mut ret = Self::ConstructedCompressor {
             m64: additional_args.4,
             mf64: additional_args.5,
             mfv: additional_args.6,
             mhl: additional_args.7,
             mhc: additional_args.8,
             mhd: additional_args.9,
             mhp: additional_args.10,
             mct: additional_args.11,
             mht: additional_args.12,
             mzn: additional_args.13,
             brotli_data: ResizableByteBuffer::<u8, AllocU8>::new(),
             divans_data: ResizableByteBuffer::<u8, AllocU8>::new(),
             encoded_byte_offset:0, 
             brotli_encoder: brotli::enc::encode::BrotliEncoderCreateInstance(additional_args.0,
                                                                              additional_args.1,
                                                                              additional_args.2,
                                                                              m32,
                                                                              additional_args.3),
            codec:DivansCodec::<Self::DefaultEncoder, EncoderSpecialization, interface::DefaultCDF16, AllocU8, AllocCDF2, AllocCDF16>::new(
                m8,
                mcdf2,
                mcdf16,
                enc,
                EncoderSpecialization::new(),
                window_size as usize,
                opt.dynamic_context_mixing.unwrap_or(0),
                opt.prior_depth,
                opt.literal_adaptation,
                opt.use_context_map,
                opt.force_stride_value,
            ),
            header_progress: 0,
            window_size: window_size as u8,
        };
        brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                       brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_LGWIN,
                                                       window_size as u32);
        brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                       brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_LGBLOCK,
                                                       opt.lgblock.unwrap_or(18));
        brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                       brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_QUALITY,
                                                       u32::from(opt.quality.unwrap_or(10)));
        brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                       brotli::enc::encode::BrotliEncoderParameter::BROTLI_METABLOCK_CALLBACK,
                                                       1);
        brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                       brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_CDF_ADAPTATION_DETECTION,
                                                       u32::from(opt.speed_detection_quality.unwrap_or(0)));
        brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                       brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_STRIDE_DETECTION_QUALITY,
                                                       u32::from(opt.stride_detection_quality.unwrap_or(0)));
        if let Some(literal_byte_score) = opt.brotli_literal_byte_score {
            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_LITERAL_BYTE_SCORE,
                                                           literal_byte_score);
        }
        
        brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                       brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_PRIOR_BITMASK_DETECTION,
                                                       u32::from(opt.prior_bitmask_detection));
        if let Some(speed) = opt.literal_adaptation {

            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_CM_SPEED,
                                                           speed[3].inc() as u32);
            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_CM_SPEED_MAX,
                                                           speed[3].lim() as u32);
            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_CM_SPEED_LOW,
                                                           speed[2].inc() as u32);
            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_CM_SPEED_LOW_MAX,
                                                           speed[2].lim() as u32);
            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_SPEED,
                                                           speed[1].inc() as u32);
            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_SPEED_MAX,
                                                           speed[1].lim() as u32);
            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_SPEED_LOW,
                                                           speed[0].inc() as u32);
            brotli::enc::encode::BrotliEncoderSetParameter(&mut ret.brotli_encoder,
                                                           brotli::enc::encode::BrotliEncoderParameter::BROTLI_PARAM_SPEED_LOW_MAX,
                                                           speed[0].lim() as u32);
        }
        ret
    }
}
