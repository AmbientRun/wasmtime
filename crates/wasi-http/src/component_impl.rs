use crate::wasi::http::types::{Error, Method, RequestOptions, Scheme};
use crate::{WasiHttpView, WasiHttpViewExt};
use anyhow::anyhow;
use std::str;
use std::vec::Vec;
use wasmtime::{AsContext, AsContextMut, Caller, Extern, Memory};
use wasmtime_wasi::preview2::bindings::{io, poll};

const MEMORY: &str = "memory";

#[derive(Debug, thiserror::Error)]
enum HttpError {
    #[error("Memory not found")]
    MemoryNotFound,
    #[error("Memory access error")]
    MemoryAccessError(#[from] wasmtime::MemoryAccessError),
    #[error("Buffer too small")]
    BufferTooSmall,
    #[error("UTF-8 error")]
    Utf8Error(#[from] std::str::Utf8Error),
}

fn memory_get<T>(caller: &mut Caller<'_, T>) -> Result<Memory, HttpError> {
    if let Some(Extern::Memory(mem)) = caller.get_export(MEMORY) {
        Ok(mem)
    } else {
        Err(HttpError::MemoryNotFound)
    }
}

/// Get a slice of length `len` from `memory`, starting at `offset`.
/// This will return an `HttpError::BufferTooSmall` if the size of the
/// requested slice is larger than the memory size.
fn slice_from_memory(
    memory: &Memory,
    mut ctx: impl AsContextMut,
    offset: u32,
    len: u32,
) -> Result<Vec<u8>, HttpError> {
    let required_memory_size = offset.checked_add(len).ok_or(HttpError::BufferTooSmall)? as usize;

    if required_memory_size > memory.data_size(&mut ctx) {
        return Err(HttpError::BufferTooSmall);
    }

    let mut buf = vec![0u8; len as usize];
    memory.read(&mut ctx, offset as usize, buf.as_mut_slice())?;
    Ok(buf)
}

fn u32_from_memory(memory: &Memory, ctx: impl AsContextMut, ptr: u32) -> Result<u32, HttpError> {
    let slice = slice_from_memory(memory, ctx, ptr, 4)?;
    let mut dst = [0u8; 4];
    dst.clone_from_slice(&slice[0..4]);
    Ok(u32::from_le_bytes(dst))
}

/// Read a string of byte length `len` from `memory`, starting at `offset`.
fn string_from_memory(
    memory: &Memory,
    ctx: impl AsContextMut,
    offset: u32,
    len: u32,
) -> Result<String, HttpError> {
    let slice = slice_from_memory(memory, ctx, offset, len)?;
    Ok(std::str::from_utf8(&slice)?.to_string())
}

fn read_option_string(
    memory: &Memory,
    ctx: impl AsContextMut,
    is_some: i32,
    ptr: u32,
    len: u32,
) -> Result<Option<String>, HttpError> {
    if is_some == 1 {
        Ok(Some(string_from_memory(&memory, ctx, ptr, len)?))
    } else {
        Ok(None)
    }
}

async fn allocate_guest_pointer<T: Send>(
    caller: &mut Caller<'_, T>,
    size: u32,
) -> anyhow::Result<u32> {
    let realloc = caller
        .get_export("cabi_realloc")
        .ok_or_else(|| anyhow!("missing required export cabi_realloc"))?;
    let func = realloc
        .into_func()
        .ok_or_else(|| anyhow!("cabi_realloc must be a func"))?;
    let typed = func.typed::<(u32, u32, u32, u32), u32>(caller.as_context())?;
    Ok(typed
        .call_async(caller.as_context_mut(), (0, 0, 4, size))
        .await?)
}

fn u32_array_to_u8(arr: &[u32]) -> Vec<u8> {
    let mut result = std::vec::Vec::new();
    for val in arr.iter() {
        let bytes = val.to_le_bytes();
        for b in bytes.iter() {
            result.push(*b);
        }
    }
    result
}

pub fn add_component_to_linker<T>(
    linker: &mut wasmtime::Linker<T>,
    get_cx: impl Fn(&mut T) -> &mut T + Send + Sync + Copy + 'static,
) -> anyhow::Result<()>
where
    T: WasiHttpView
        + WasiHttpViewExt
        + crate::wasi::http::outgoing_handler::Host
        + crate::wasi::http::types::Host
        + io::streams::Host
        + poll::poll::Host,
{
    linker.func_wrap8_async(
        "wasi:http/outgoing-handler",
        "handle",
        move |mut caller: Caller<'_, T>,
              request: u32,
              has_options: i32,
              has_timeout: i32,
              timeout_ms: u32,
              has_first_byte_timeout: i32,
              first_byte_timeout_ms: u32,
              has_between_bytes_timeout: i32,
              between_bytes_timeout_ms: u32| {
            Box::new(async move {
                let options = if has_options == 1 {
                    Some(RequestOptions {
                        connect_timeout_ms: if has_timeout == 1 {
                            Some(timeout_ms)
                        } else {
                            None
                        },
                        first_byte_timeout_ms: if has_first_byte_timeout == 1 {
                            Some(first_byte_timeout_ms)
                        } else {
                            None
                        },
                        between_bytes_timeout_ms: if has_between_bytes_timeout == 1 {
                            Some(between_bytes_timeout_ms)
                        } else {
                            None
                        },
                    })
                } else {
                    None
                };

                get_cx(caller.data_mut()).handle(request, options).await
            })
        },
    )?;
    linker.func_wrap14_async(
        "wasi:http/types",
        "new-outgoing-request",
        move |mut caller: Caller<'_, T>,
              method: i32,
              method_ptr: i32,
              method_len: i32,
              path_is_some: i32,
              path_ptr: u32,
              path_len: u32,
              scheme_is_some: i32,
              scheme: i32,
              scheme_ptr: i32,
              scheme_len: i32,
              authority_is_some: i32,
              authority_ptr: u32,
              authority_len: u32,
              headers: u32| {
            Box::new(async move {
                let memory = memory_get(&mut caller)?;
                let path = read_option_string(
                    &memory,
                    caller.as_context_mut(),
                    path_is_some,
                    path_ptr,
                    path_len,
                )?;
                let authority = read_option_string(
                    &memory,
                    caller.as_context_mut(),
                    authority_is_some,
                    authority_ptr,
                    authority_len,
                )?;

                let mut s = Scheme::Https;
                if scheme_is_some == 1 {
                    s = match scheme {
                        0 => Scheme::Http,
                        1 => Scheme::Https,
                        _ => {
                            let value = string_from_memory(
                                &memory,
                                caller.as_context_mut(),
                                scheme_ptr.try_into()?,
                                scheme_len.try_into()?,
                            )?;
                            Scheme::Other(value)
                        }
                    };
                }
                let m = match method {
                    0 => Method::Get,
                    1 => Method::Head,
                    2 => Method::Post,
                    3 => Method::Put,
                    4 => Method::Delete,
                    5 => Method::Connect,
                    6 => Method::Options,
                    7 => Method::Trace,
                    8 => Method::Patch,
                    _ => {
                        let value = string_from_memory(
                            &memory,
                            caller.as_context_mut(),
                            method_ptr.try_into()?,
                            method_len.try_into()?,
                        )?;
                        Method::Other(value)
                    }
                };

                let ctx = get_cx(caller.data_mut());
                ctx.new_outgoing_request(m, path, Some(s), authority, headers)
                    .await
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:http/types",
        "incoming-response-status",
        move |mut caller: Caller<'_, T>, id: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                let result: u32 = ctx.incoming_response_status(id).await?.into();
                Ok(result)
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:http/types",
        "drop-future-incoming-response",
        move |mut caller: Caller<'_, T>, future: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                ctx.drop_future_incoming_response(future).await
            })
        },
    )?;
    linker.func_wrap2_async(
        "wasi:http/types",
        "future-incoming-response-get",
        move |mut caller: Caller<'_, T>, future: u32, ptr: i32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                let response = ctx.future_incoming_response_get(future).await?;

                let memory = memory_get(&mut caller)?;

                // First == is_some
                // Second == is_err
                // Third == {ok: is_err = false, tag: is_err = true}
                // Fourth == string ptr
                // Fifth == string len
                let result: [u32; 5] = match response {
                    Some(inner) => match inner {
                        Ok(value) => [1, 0, value, 0, 0],
                        Err(error) => {
                            let (tag, err_string) = match error {
                                Error::InvalidUrl(e) => (0u32, e),
                                Error::TimeoutError(e) => (1u32, e),
                                Error::ProtocolError(e) => (2u32, e),
                                Error::UnexpectedError(e) => (3u32, e),
                            };
                            let bytes = err_string.as_bytes();
                            let len = bytes.len().try_into().unwrap();
                            let ptr = allocate_guest_pointer(&mut caller, len).await?;
                            memory.write(caller.as_context_mut(), ptr as _, bytes)?;
                            [1, 1, tag, ptr, len]
                        }
                    },
                    None => [0, 0, 0, 0, 0],
                };
                let raw = u32_array_to_u8(&result);

                memory.write(caller.as_context_mut(), ptr as _, &raw)?;
                Ok(())
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:http/types",
        "listen-to-future-incoming-response",
        move |mut caller: Caller<'_, T>, future: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                ctx.listen_to_future_incoming_response(future).await
            })
        },
    )?;
    linker.func_wrap2_async(
        "wasi:http/types",
        "incoming-response-consume",
        move |mut caller: Caller<'_, T>, response: u32, ptr: i32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                let stream = ctx.incoming_response_consume(response).await?.unwrap_or(0);

                let memory = memory_get(&mut caller).unwrap();

                // First == is_some
                // Second == stream_id
                // let result: [u32; 2] = match result {
                //     Ok(value) => [0, value],
                //     Err(_) => [1, 0],
                // };
                let result: [u32; 2] = [0, stream];
                let raw = u32_array_to_u8(&result);

                memory.write(caller.as_context_mut(), ptr as _, &raw)?;
                Ok(())
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:poll/poll",
        "drop-pollable",
        move |mut caller: Caller<'_, T>, id: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                poll::poll::Host::drop_pollable(ctx, id).await
            })
        },
    )?;
    linker.func_wrap3_async(
        "wasi:poll/poll",
        "poll-oneoff",
        move |mut caller: Caller<'_, T>, base_ptr: u32, len: u32, out_ptr: u32| {
            Box::new(async move {
                let memory = memory_get(&mut caller)?;

                let mut vec = Vec::new();
                let mut i = 0;
                while i < len {
                    let ptr = base_ptr + i * 4;
                    let pollable_ptr = u32_from_memory(&memory, caller.as_context_mut(), ptr)?;
                    vec.push(pollable_ptr);
                    i = i + 1;
                }

                let ctx = get_cx(caller.data_mut());
                let result = poll::poll::Host::poll_oneoff(ctx, vec).await?;

                let result_len = result.len();
                let result_ptr =
                    allocate_guest_pointer(&mut caller, (4 * result_len).try_into()?).await?;
                let mut ptr = result_ptr;
                for item in result.iter() {
                    let completion: u32 = match item {
                        true => 1,
                        false => 0,
                    };
                    memory.write(caller.as_context_mut(), ptr as _, &completion.to_be_bytes())?;

                    ptr = ptr + 4;
                }

                let result: [u32; 2] = [result_ptr, result_len.try_into()?];
                let raw = u32_array_to_u8(&result);
                memory.write(caller.as_context_mut(), out_ptr as _, &raw)?;
                Ok(())
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:io/streams",
        "drop-input-stream",
        move |mut caller: Caller<'_, T>, id: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                io::streams::Host::drop_input_stream(ctx, id).await
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:io/streams",
        "drop-output-stream",
        move |mut caller: Caller<'_, T>, id: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                io::streams::Host::drop_output_stream(ctx, id).await
            })
        },
    )?;
    linker.func_wrap3_async(
        "wasi:io/streams",
        "read",
        move |mut caller: Caller<'_, T>, stream: u32, len: u64, ptr: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());

                let (bytes, status) = io::streams::Host::read(ctx, stream, len)
                    .await?
                    .map_err(|_| anyhow!("read failed"))?;

                let done = match status {
                    io::streams::StreamStatus::Open => 0,
                    io::streams::StreamStatus::Ended => 1,
                };
                let body_len: u32 = bytes.len().try_into()?;
                let out_ptr = allocate_guest_pointer(&mut caller, body_len).await?;

                // First == is_err
                // Second == {ok: is_err = false, tag: is_err = true}
                // Third == bytes length
                // Fourth == enum status
                let result: [u32; 4] = [0, out_ptr, body_len, done];
                let raw = u32_array_to_u8(&result);

                let memory = memory_get(&mut caller)?;
                memory.write(caller.as_context_mut(), out_ptr as _, &bytes)?;
                memory.write(caller.as_context_mut(), ptr as _, &raw)?;
                Ok(())
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:io/streams",
        "subscribe-to-input-stream",
        move |mut caller: Caller<'_, T>, stream: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                io::streams::Host::subscribe_to_input_stream(ctx, stream).await
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:io/streams",
        "subscribe-to-output-stream",
        move |mut caller: Caller<'_, T>, stream: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                io::streams::Host::subscribe_to_output_stream(ctx, stream).await
            })
        },
    )?;
    linker.func_wrap4_async(
        "wasi:io/streams",
        "write",
        move |mut caller: Caller<'_, T>, stream: u32, body_ptr: u32, body_len: u32, ptr: u32| {
            Box::new(async move {
                let memory: Memory = memory_get(&mut caller)?;
                let body =
                    string_from_memory(&memory, caller.as_context_mut(), body_ptr, body_len)?;

                let ctx = get_cx(caller.data_mut());

                let (len, status) = io::streams::Host::write(ctx, stream, body.into())
                    .await?
                    .map_err(|_| anyhow!("write failed"))?;
                let written: u32 = len.try_into()?;
                let done: u32 = match status {
                    io::streams::StreamStatus::Open => 0,
                    io::streams::StreamStatus::Ended => 1,
                };

                // First == is_err
                // Second == {ok: is_err = false, tag: is_err = true}
                // Third == amount of bytes written
                // Fifth == enum status
                let result: [u32; 5] = [0, 0, written, 0, done];
                let raw = u32_array_to_u8(&result);

                memory.write(caller.as_context_mut(), ptr as _, &raw)?;

                Ok(())
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:http/types",
        "drop-fields",
        move |mut caller: Caller<'_, T>, ptr: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                ctx.drop_fields(ptr).await
            })
        },
    )?;
    linker.func_wrap2_async(
        "wasi:http/types",
        "outgoing-request-write",
        move |mut caller: Caller<'_, T>, request: u32, ptr: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                let stream = ctx
                    .outgoing_request_write(request)
                    .await?
                    .map_err(|_| anyhow!("no outgoing stream present"))?;

                let memory = memory_get(&mut caller)?;
                // First == is_some
                // Second == stream_id
                let result: [u32; 2] = [0, stream];
                let raw = u32_array_to_u8(&result);

                memory.write(caller.as_context_mut(), ptr as _, &raw)?;
                Ok(())
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:http/types",
        "drop-outgoing-request",
        move |mut caller: Caller<'_, T>, id: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                ctx.drop_outgoing_request(id).await
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:http/types",
        "drop-incoming-response",
        move |mut caller: Caller<'_, T>, id: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                ctx.drop_incoming_response(id).await
            })
        },
    )?;
    linker.func_wrap2_async(
        "wasi:http/types",
        "new-fields",
        move |mut caller: Caller<'_, T>, base_ptr: u32, len: u32| {
            Box::new(async move {
                let memory = memory_get(&mut caller)?;

                let mut vec = Vec::new();
                let mut i = 0;
                // TODO: read this more efficiently as a single block.
                while i < len {
                    let ptr = base_ptr + i * 16;
                    let name_ptr = u32_from_memory(&memory, caller.as_context_mut(), ptr)?;
                    let name_len = u32_from_memory(&memory, caller.as_context_mut(), ptr + 4)?;
                    let value_ptr = u32_from_memory(&memory, caller.as_context_mut(), ptr + 8)?;
                    let value_len = u32_from_memory(&memory, caller.as_context_mut(), ptr + 12)?;

                    let name =
                        string_from_memory(&memory, caller.as_context_mut(), name_ptr, name_len)?;
                    let value =
                        string_from_memory(&memory, caller.as_context_mut(), value_ptr, value_len)?;

                    vec.push((name, value));
                    i = i + 1;
                }

                let ctx = get_cx(caller.data_mut());
                ctx.new_fields(vec).await
            })
        },
    )?;
    linker.func_wrap2_async(
        "wasi:http/types",
        "fields-entries",
        move |mut caller: Caller<'_, T>, fields: u32, out_ptr: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                let entries = ctx.fields_entries(fields).await?;

                let header_len = entries.len();
                let tuple_ptr =
                    allocate_guest_pointer(&mut caller, (16 * header_len).try_into()?).await?;
                let mut ptr = tuple_ptr;
                for item in entries.iter() {
                    let name = &item.0;
                    let value = &item.1;
                    let name_len: u32 = name.len().try_into()?;
                    let value_len: u32 = value.len().try_into()?;

                    let name_ptr = allocate_guest_pointer(&mut caller, name_len).await?;
                    let value_ptr = allocate_guest_pointer(&mut caller, value_len).await?;

                    let memory = memory_get(&mut caller)?;
                    memory.write(caller.as_context_mut(), name_ptr as _, &name.as_bytes())?;
                    memory.write(caller.as_context_mut(), value_ptr as _, value)?;

                    let pair: [u32; 4] = [name_ptr, name_len, value_ptr, value_len];
                    let raw_pair = u32_array_to_u8(&pair);
                    memory.write(caller.as_context_mut(), ptr as _, &raw_pair)?;

                    ptr = ptr + 16;
                }

                let memory = memory_get(&mut caller)?;
                let result: [u32; 2] = [tuple_ptr, header_len.try_into()?];
                let raw = u32_array_to_u8(&result);
                memory.write(caller.as_context_mut(), out_ptr as _, &raw)?;
                Ok(())
            })
        },
    )?;
    linker.func_wrap1_async(
        "wasi:http/types",
        "incoming-response-headers",
        move |mut caller: Caller<'_, T>, handle: u32| {
            Box::new(async move {
                let ctx = get_cx(caller.data_mut());
                ctx.incoming_response_headers(handle).await
            })
        },
    )?;
    Ok(())
}
