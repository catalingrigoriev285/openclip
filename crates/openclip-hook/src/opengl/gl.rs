//! The modern-OpenGL entry points the `windows` crate does not bind, and the
//! two ways of drawing a textured quad with them.
//!
//! The crate binds OpenGL 1.1 and nothing else — that is genuinely all
//! `opengl32.dll` exports, everything newer has to come through
//! `wglGetProcAddress`. For a compatibility-profile context 1.1 is enough and
//! [`Painter::Immediate`] uses it. For a **core** profile it is not: `glBegin`,
//! `glOrtho`, `glPushAttrib` and client-side arrays were all removed in 3.2, so
//! [`Painter::Shader`] builds a tiny program instead and drives it from an empty
//! vertex array, taking the quad's corners from `gl_VertexID` the same way the
//! Direct3D path takes them from `SV_VertexID`.
//!
//! `wglGetProcAddress` returns null without a current context and its results
//! belong to that context, so everything here is resolved lazily on the game's
//! render thread with its context current — never from the worker.

use windows::core::PCSTR;
use windows::Win32::Graphics::OpenGL::*;

// Constants from GL 1.2 and later, which the crate does not carry.
pub const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_ARRAY_BUFFER: u32 = 0x8892;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_INFO_LOG_LENGTH: u32 = 0x8B84;
const GL_CURRENT_PROGRAM: u32 = 0x8B8D;
const GL_VERTEX_ARRAY_BINDING: u32 = 0x85B5;
const GL_ARRAY_BUFFER_BINDING: u32 = 0x8894;
const GL_ACTIVE_TEXTURE: u32 = 0x84E0;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_BLEND_SRC_RGB: u32 = 0x80C9;
const GL_BLEND_DST_RGB: u32 = 0x80C8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
const GL_SCISSOR_TEST: u32 = 0x0C11;
const GL_SAMPLER_BINDING: u32 = 0x8919;
const GL_STENCIL_TEST: u32 = 0x0B90;
const GL_COLOR_WRITEMASK: u32 = 0x0C23;
const GL_IMPLEMENTATION_COLOR_READ_TYPE: u32 = 0x8B9A;
const GL_IMPLEMENTATION_COLOR_READ_FORMAT: u32 = 0x8B9B;
const GL_SAMPLE_BUFFERS: u32 = 0x80A8;
const GL_PIXEL_UNPACK_BUFFER: u32 = 0x88EC;
const GL_PIXEL_UNPACK_BUFFER_BINDING: u32 = 0x8895;
const GL_PIXEL_PACK_BUFFER: u32 = 0x88EB;
const GL_PIXEL_PACK_BUFFER_BINDING: u32 = 0x88ED;
const GL_UNPACK_ROW_LENGTH: u32 = 0x0CF2;
const GL_UNPACK_SKIP_ROWS: u32 = 0x0CF3;
const GL_UNPACK_SKIP_PIXELS: u32 = 0x0CF4;
const GL_UNPACK_IMAGE_HEIGHT: u32 = 0x806E;
const GL_PACK_ROW_LENGTH: u32 = 0x0D02;
const GL_PACK_SKIP_ROWS: u32 = 0x0D03;
const GL_PACK_SKIP_PIXELS: u32 = 0x0D04;
pub const GL_TEXTURE_BASE_LEVEL: u32 = 0x813C;
pub const GL_TEXTURE_MAX_LEVEL: u32 = 0x813D;

type GlCreateShader = unsafe extern "system" fn(u32) -> u32;
type GlShaderSource = unsafe extern "system" fn(u32, i32, *const *const u8, *const i32);
type GlCompileShader = unsafe extern "system" fn(u32);
type GlGetShaderiv = unsafe extern "system" fn(u32, u32, *mut i32);
type GlGetShaderInfoLog = unsafe extern "system" fn(u32, i32, *mut i32, *mut u8);
type GlCreateProgram = unsafe extern "system" fn() -> u32;
type GlAttachShader = unsafe extern "system" fn(u32, u32);
type GlLinkProgram = unsafe extern "system" fn(u32);
type GlGetProgramiv = unsafe extern "system" fn(u32, u32, *mut i32);
type GlUseProgram = unsafe extern "system" fn(u32);
type GlDeleteShader = unsafe extern "system" fn(u32);
type GlDeleteProgram = unsafe extern "system" fn(u32);
type GlGetUniformLocation = unsafe extern "system" fn(u32, *const u8) -> i32;
type GlUniform4f = unsafe extern "system" fn(i32, f32, f32, f32, f32);
type GlUniform1i = unsafe extern "system" fn(i32, i32);
type GlGenVertexArrays = unsafe extern "system" fn(i32, *mut u32);
type GlBindVertexArray = unsafe extern "system" fn(u32);
type GlDeleteVertexArrays = unsafe extern "system" fn(i32, *const u32);
type GlActiveTexture = unsafe extern "system" fn(u32);
type GlBindFramebuffer = unsafe extern "system" fn(u32, u32);
type GlBindBuffer = unsafe extern "system" fn(u32, u32);
type GlBindSampler = unsafe extern "system" fn(u32, u32);

/// Every entry point past 1.1 that the overlay needs, resolved once per context.
///
/// All optional: a 1.1 or 2.1 context has none of the vertex-array functions,
/// and the painter falls back to immediate mode rather than refusing to draw.
#[derive(Default)]
pub struct Ext {
    create_shader: Option<GlCreateShader>,
    shader_source: Option<GlShaderSource>,
    compile_shader: Option<GlCompileShader>,
    get_shaderiv: Option<GlGetShaderiv>,
    get_shader_info_log: Option<GlGetShaderInfoLog>,
    create_program: Option<GlCreateProgram>,
    attach_shader: Option<GlAttachShader>,
    link_program: Option<GlLinkProgram>,
    get_programiv: Option<GlGetProgramiv>,
    use_program: Option<GlUseProgram>,
    delete_shader: Option<GlDeleteShader>,
    delete_program: Option<GlDeleteProgram>,
    get_uniform_location: Option<GlGetUniformLocation>,
    uniform4f: Option<GlUniform4f>,
    uniform1i: Option<GlUniform1i>,
    gen_vertex_arrays: Option<GlGenVertexArrays>,
    bind_vertex_array: Option<GlBindVertexArray>,
    delete_vertex_arrays: Option<GlDeleteVertexArrays>,
    active_texture: Option<GlActiveTexture>,
    bind_framebuffer: Option<GlBindFramebuffer>,
    bind_buffer: Option<GlBindBuffer>,
    /// GL 3.3. A sampler object bound to the unit **overrides** the texture's own
    /// filter and wrap parameters, so one left bound by the game with a mipmap
    /// min-filter makes our single-level badge texture *incomplete* — and an
    /// incomplete texture samples as opaque black. That is exactly the black
    /// rectangle Minecraft showed instead of the counter.
    bind_sampler: Option<GlBindSampler>,
}

/// Resolves one entry point. `None` when the context does not have it.
///
/// # Safety
/// The returned pointer is called with the signature `T` spells, so `T` must
/// match the OpenGL prototype for `name`.
unsafe fn proc_address<T>(name: &[u8]) -> Option<T> {
    debug_assert_eq!(name.last(), Some(&0), "wglGetProcAddress takes a C string");
    // SAFETY: a NUL-terminated name; the transmute is the caller's obligation.
    unsafe {
        let f = wglGetProcAddress(PCSTR(name.as_ptr()))?;
        Some(std::mem::transmute_copy::<unsafe extern "system" fn() -> isize, T>(&f))
    }
}

impl Ext {
    /// Must be called on the render thread, with the game's context current.
    pub fn load() -> Self {
        // SAFETY: every transmute below pairs a name with the fn-pointer type
        // spelling that function's OpenGL prototype.
        unsafe {
            Self {
                create_shader: proc_address(b"glCreateShader\0"),
                shader_source: proc_address(b"glShaderSource\0"),
                compile_shader: proc_address(b"glCompileShader\0"),
                get_shaderiv: proc_address(b"glGetShaderiv\0"),
                get_shader_info_log: proc_address(b"glGetShaderInfoLog\0"),
                create_program: proc_address(b"glCreateProgram\0"),
                attach_shader: proc_address(b"glAttachShader\0"),
                link_program: proc_address(b"glLinkProgram\0"),
                get_programiv: proc_address(b"glGetProgramiv\0"),
                use_program: proc_address(b"glUseProgram\0"),
                delete_shader: proc_address(b"glDeleteShader\0"),
                delete_program: proc_address(b"glDeleteProgram\0"),
                get_uniform_location: proc_address(b"glGetUniformLocation\0"),
                uniform4f: proc_address(b"glUniform4f\0"),
                uniform1i: proc_address(b"glUniform1i\0"),
                gen_vertex_arrays: proc_address(b"glGenVertexArrays\0"),
                bind_vertex_array: proc_address(b"glBindVertexArray\0"),
                delete_vertex_arrays: proc_address(b"glDeleteVertexArrays\0"),
                active_texture: proc_address(b"glActiveTexture\0"),
                bind_framebuffer: proc_address(b"glBindFramebuffer\0"),
                bind_buffer: proc_address(b"glBindBuffer\0"),
                bind_sampler: proc_address(b"glBindSampler\0"),
            }
        }
    }

    /// Whether a shader program can be built at all.
    fn has_shaders(&self) -> bool {
        self.create_shader.is_some()
            && self.shader_source.is_some()
            && self.compile_shader.is_some()
            && self.create_program.is_some()
            && self.attach_shader.is_some()
            && self.link_program.is_some()
            && self.use_program.is_some()
            && self.get_uniform_location.is_some()
            && self.uniform4f.is_some()
            && self.uniform1i.is_some()
            && self.gen_vertex_arrays.is_some()
            && self.bind_vertex_array.is_some()
            && self.active_texture.is_some()
    }

    /// The framebuffer currently bound for reading, so the readback can put it
    /// back. Zero when the context has no framebuffer objects at all.
    ///
    /// # Safety
    /// Must run on the render thread with the game's context current.
    pub unsafe fn read_framebuffer(&self) -> u32 {
        if self.bind_framebuffer.is_none() {
            return 0;
        }
        let mut v = 0i32;
        // SAFETY: the caller guarantees a current context.
        unsafe { glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &mut v) };
        v as u32
    }

    /// Points the read framebuffer at `name`, if the context has framebuffers at
    /// all. A 1.1 context has only the default one, so there is nothing to do.
    ///
    /// # Safety
    /// Must run on the render thread with the game's context current.
    pub unsafe fn bind_read_framebuffer(&self, name: u32) {
        if let Some(f) = self.bind_framebuffer {
            // SAFETY: the caller guarantees a current context.
            unsafe { f(GL_READ_FRAMEBUFFER, name) };
        }
    }
}

/// The pixel-store state around a transfer, saved so the game gets it back.
///
/// A real engine leaves these set: Minecraft uploads sub-rectangles of atlases
/// with `GL_UNPACK_ROW_LENGTH` and the skip parameters, and a leftover pixel
/// buffer binding turns the pointer handed to `glTexImage2D` or `glReadPixels`
/// into an *offset into that buffer* — which silently transfers the wrong
/// memory rather than failing. Neither is something an overlay may inherit.
pub struct PixelStore {
    unpack: bool,
    row_length: i32,
    skip_rows: i32,
    skip_pixels: i32,
    image_height: i32,
    alignment: i32,
    buffer: i32,
}

impl PixelStore {
    /// Saves and neutralises the unpack (upload) state.
    ///
    /// # Safety
    /// Current context, render thread.
    pub unsafe fn take_unpack(ext: &Ext, alignment: i32) -> Self {
        // SAFETY: plain queries and sets on the current context.
        unsafe { Self::take(ext, true, alignment) }
    }

    /// Saves and neutralises the pack (readback) state.
    ///
    /// # Safety
    /// Current context, render thread.
    pub unsafe fn take_pack(ext: &Ext, alignment: i32) -> Self {
        // SAFETY: as above.
        unsafe { Self::take(ext, false, alignment) }
    }

    unsafe fn take(ext: &Ext, unpack: bool, alignment: i32) -> Self {
        unsafe {
            let get = |p: u32| {
                let mut v = 0i32;
                glGetIntegerv(p, &mut v);
                v
            };
            let (row, skip_r, skip_p, align, binding, target) = if unpack {
                (
                    GL_UNPACK_ROW_LENGTH,
                    GL_UNPACK_SKIP_ROWS,
                    GL_UNPACK_SKIP_PIXELS,
                    GL_UNPACK_ALIGNMENT,
                    GL_PIXEL_UNPACK_BUFFER_BINDING,
                    GL_PIXEL_UNPACK_BUFFER,
                )
            } else {
                (
                    GL_PACK_ROW_LENGTH,
                    GL_PACK_SKIP_ROWS,
                    GL_PACK_SKIP_PIXELS,
                    GL_PACK_ALIGNMENT,
                    GL_PIXEL_PACK_BUFFER_BINDING,
                    GL_PIXEL_PACK_BUFFER,
                )
            };
            let saved = Self {
                unpack,
                row_length: get(row),
                skip_rows: get(skip_r),
                skip_pixels: get(skip_p),
                // Only the unpack side has an image height, and only from 1.2.
                image_height: if unpack && ext.bind_buffer.is_some() { get(GL_UNPACK_IMAGE_HEIGHT) } else { 0 },
                alignment: get(align),
                buffer: if ext.bind_buffer.is_some() { get(binding) } else { 0 },
            };
            glPixelStorei(row, 0);
            glPixelStorei(skip_r, 0);
            glPixelStorei(skip_p, 0);
            if unpack && ext.bind_buffer.is_some() {
                glPixelStorei(GL_UNPACK_IMAGE_HEIGHT, 0);
            }
            glPixelStorei(align, alignment);
            if let Some(f) = ext.bind_buffer {
                f(target, 0);
            }
            saved
        }
    }

    /// # Safety
    /// Same context [`take_unpack`](Self::take_unpack) read.
    pub unsafe fn restore(self, ext: &Ext) {
        unsafe {
            let (row, skip_r, skip_p, align, target) = if self.unpack {
                (
                    GL_UNPACK_ROW_LENGTH,
                    GL_UNPACK_SKIP_ROWS,
                    GL_UNPACK_SKIP_PIXELS,
                    GL_UNPACK_ALIGNMENT,
                    GL_PIXEL_UNPACK_BUFFER,
                )
            } else {
                (GL_PACK_ROW_LENGTH, GL_PACK_SKIP_ROWS, GL_PACK_SKIP_PIXELS, GL_PACK_ALIGNMENT, GL_PIXEL_PACK_BUFFER)
            };
            glPixelStorei(row, self.row_length);
            glPixelStorei(skip_r, self.skip_rows);
            glPixelStorei(skip_p, self.skip_pixels);
            if self.unpack && ext.bind_buffer.is_some() {
                glPixelStorei(GL_UNPACK_IMAGE_HEIGHT, self.image_height);
            }
            glPixelStorei(align, self.alignment);
            if let Some(f) = ext.bind_buffer {
                f(target, self.buffer as u32);
            }
        }
    }
}

/// The pixel format [`glReadPixels`] may legally be asked for.
///
/// This is the fix for a hard crash, not an optimisation. A **core** profile
/// guarantees exactly two accepted combinations: `GL_RGBA`/`GL_UNSIGNED_BYTE`,
/// and whatever pair the implementation advertises through
/// `GL_IMPLEMENTATION_COLOR_READ_FORMAT`/`_TYPE`. Anything else — `GL_BGRA`
/// included — raises `GL_INVALID_OPERATION`, and a game that checks
/// `glGetError` every frame (Minecraft throws `IllegalStateException` on one)
/// dies on the spot. So the fast path is *asked for*, never assumed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ReadFormat {
    /// The `format` argument to pass to `glReadPixels`.
    pub gl_format: u32,
    /// Whether the bytes come back blue-first, which decides the DXGI format
    /// the staging texture is created with.
    pub bgra: bool,
    /// Whether the default framebuffer can be read at all.
    ///
    /// `glReadPixels` on a **multisampled** framebuffer is another
    /// `GL_INVALID_OPERATION`, and resolving one needs a whole blit-to-single-
    /// sample path. Detecting it and declining to capture keeps the game alive
    /// and the counter working, which beats crashing it.
    pub readable: bool,
}

impl ReadFormat {
    /// Asks the implementation what it will accept, preferring BGRA because it
    /// matches the shared texture the rest of the transport uses and so avoids
    /// a channel swap on the render thread.
    ///
    /// # Safety
    /// Current context, render thread.
    pub unsafe fn negotiate() -> Self {
        // SAFETY: plain queries on the current context.
        unsafe {
            let mut format = 0i32;
            let mut kind = 0i32;
            glGetIntegerv(GL_IMPLEMENTATION_COLOR_READ_FORMAT, &mut format);
            glGetIntegerv(GL_IMPLEMENTATION_COLOR_READ_TYPE, &mut kind);
            // Those two queries are themselves only guaranteed from GL 4.1 /
            // ES 2.0; on an older context they leave the values unset and
            // raise GL_INVALID_ENUM, which the drain below clears.
            let mut samples = 0i32;
            glGetIntegerv(GL_SAMPLE_BUFFERS, &mut samples);
            drain_errors();
            let readable = samples == 0;
            if format as u32 == GL_BGRA_EXT && kind as u32 == GL_UNSIGNED_BYTE {
                return Self { gl_format: GL_BGRA_EXT, bgra: true, readable };
            }
            // The one combination every profile must accept.
            Self { gl_format: GL_RGBA, bgra: false, readable }
        }
    }
}

/// Swallows any error this overlay provoked.
///
/// Not tidiness: a game that calls `glGetError` in its own render loop will
/// attribute whatever we left behind to its own draw. Minecraft turns that into
/// a crash report, so every path that touches GL ends here.
///
/// # Safety
/// Current context, render thread.
pub unsafe fn drain_errors() {
    // Bounded: a driver that somehow never returns GL_NO_ERROR must not spin
    // inside a render thread.
    for _ in 0..16 {
        // SAFETY: the caller guarantees a current context.
        if unsafe { glGetError() } == GL_NO_ERROR {
            return;
        }
    }
}

/// A one-line description of the state the game left bound, for the log.
///
/// Sampler bindings and pixel-store leftovers are the two things that silently
/// turn an overlay into a black rectangle, so they are worth stating explicitly
/// once rather than guessing at at from a screenshot.
///
/// # Safety
/// Current context, render thread.
pub unsafe fn describe_state(ext: &Ext) -> String {
    unsafe {
        let get = |p: u32| {
            let mut v = 0i32;
            glGetIntegerv(p, &mut v);
            v
        };
        let sampler = if ext.bind_sampler.is_some() { get(GL_SAMPLER_BINDING) } else { -1 };
        format!(
            "sampler={sampler} unpack_row={} unpack_skip={},{} unpack_buffer={} pack_row={} pack_buffer={} srgb={}",
            get(GL_UNPACK_ROW_LENGTH),
            get(GL_UNPACK_SKIP_PIXELS),
            get(GL_UNPACK_SKIP_ROWS),
            if ext.bind_buffer.is_some() { get(GL_PIXEL_UNPACK_BUFFER_BINDING) } else { -1 },
            get(GL_PACK_ROW_LENGTH),
            if ext.bind_buffer.is_some() { get(GL_PIXEL_PACK_BUFFER_BINDING) } else { -1 },
            glIsEnabled(0x8DB9) != 0, // GL_FRAMEBUFFER_SRGB
        )
    }
}

/// The GL version string, for the log line that says which path was taken.
pub fn version_string() -> String {
    // SAFETY: `glGetString` on a current context returns a static C string.
    unsafe {
        let p = glGetString(GL_VERSION);
        if p.is_null() {
            return "unknown".into();
        }
        let mut len = 0;
        while *p.add(len) != 0 && len < 128 {
            len += 1;
        }
        String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned()
    }
}

// ----- drawing ---------------------------------------------------------------

const VERTEX_SHADER: &[u8] = b"#version 150\n\
uniform vec4 uRect;\n\
out vec2 vUv;\n\
void main() {\n\
    vec2 c = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));\n\
    vUv = c;\n\
    gl_Position = vec4(uRect.x + c.x * uRect.z, uRect.y + c.y * uRect.w, 0.0, 1.0);\n\
}\n\0";

const FRAGMENT_SHADER: &[u8] = b"#version 150\n\
uniform sampler2D uTex;\n\
uniform vec4 uTint;\n\
in vec2 vUv;\n\
out vec4 fragColor;\n\
void main() {\n\
    vec4 t = texture(uTex, vUv);\n\
    fragColor = vec4(t.rgb, t.a * uTint.a);\n\
}\n\0";

/// How the counter gets onto the screen.
pub enum Painter {
    /// OpenGL 3.2 core and later: a program plus an empty vertex array. The only
    /// path that works in a core profile, which is what Minecraft 1.17+ asks for.
    Shader { program: u32, vao: u32, rect: i32, tint: i32, tex: i32 },
    /// A compatibility context: fixed-function immediate mode, which needs no
    /// extensions at all.
    Immediate,
}

impl std::fmt::Display for Painter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Painter::Shader { .. } => f.write_str("shader path"),
            Painter::Immediate => f.write_str("immediate-mode path"),
        }
    }
}

impl Painter {
    pub fn new(ext: &Ext) -> Result<Self, String> {
        if !ext.has_shaders() {
            // No shader entry points means a pre-2.0 context, which is by
            // definition a compatibility one, so immediate mode is available.
            return Ok(Painter::Immediate);
        }
        // SAFETY: every call is a resolved entry point on the current context;
        // failures are checked and reported rather than assumed away.
        unsafe { Self::build(ext) }
    }

    unsafe fn build(ext: &Ext) -> Result<Self, String> {
        unsafe {
            let vs = compile(ext, GL_VERTEX_SHADER, VERTEX_SHADER)?;
            let fs = match compile(ext, GL_FRAGMENT_SHADER, FRAGMENT_SHADER) {
                Ok(fs) => fs,
                Err(e) => {
                    if let Some(d) = ext.delete_shader {
                        d(vs);
                    }
                    return Err(e);
                }
            };
            let program = (ext.create_program.expect("checked"))();
            (ext.attach_shader.expect("checked"))(program, vs);
            (ext.attach_shader.expect("checked"))(program, fs);
            (ext.link_program.expect("checked"))(program);
            if let Some(d) = ext.delete_shader {
                d(vs);
                d(fs);
            }
            let mut linked = 0;
            if let Some(g) = ext.get_programiv {
                g(program, GL_LINK_STATUS, &mut linked);
            } else {
                linked = 1; // no way to ask; assume it worked
            }
            if linked == 0 {
                if let Some(d) = ext.delete_program {
                    d(program);
                }
                return Err("the overlay shader program did not link".into());
            }

            let mut vao = 0;
            (ext.gen_vertex_arrays.expect("checked"))(1, &mut vao);
            let rect = (ext.get_uniform_location.expect("checked"))(program, c"uRect".as_ptr() as *const u8);
            let tint = (ext.get_uniform_location.expect("checked"))(program, c"uTint".as_ptr() as *const u8);
            let tex = (ext.get_uniform_location.expect("checked"))(program, c"uTex".as_ptr() as *const u8);
            Ok(Painter::Shader { program, vao, rect, tint, tex })
        }
    }

    /// Draws the badge, leaving every piece of state exactly as it was found.
    ///
    /// Getting this wrong is the classic way an overlay corrupts a game, so the
    /// set saved here is deliberately the *complete* set this function touches —
    /// not the set that happens to matter for one engine.
    ///
    /// # Safety
    /// Must run on the render thread with the game's context current.
    pub unsafe fn paint(&self, ext: &Ext, texture: u32, rect: [f32; 4], opacity: f32, size: (u32, u32)) {
        unsafe {
            // `GL_TEXTURE_2D` is only a *capability* in a compatibility profile.
            // Querying or toggling it on a core one raises GL_INVALID_ENUM, so
            // it is touched on the immediate-mode path and nowhere else.
            let fixed_function = matches!(self, Painter::Immediate);
            let saved = State::capture(ext, fixed_function);
            // The counter belongs on the screen, not in whatever offscreen
            // target the game happened to leave bound.
            if let Some(f) = ext.bind_framebuffer {
                f(GL_DRAW_FRAMEBUFFER, 0);
            }
            glViewport(0, 0, size.0 as i32, size.1 as i32);
            glEnable(GL_BLEND);
            glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
            glDisable(GL_DEPTH_TEST);
            glDisable(GL_CULL_FACE);
            glDisable(GL_SCISSOR_TEST);
            glDisable(GL_STENCIL_TEST);
            glColorMask(1, 1, 1, 1);

            match self {
                Painter::Shader { program, vao, rect: u_rect, tint, tex } => {
                    (ext.use_program.expect("built with shaders"))(*program);
                    (ext.bind_vertex_array.expect("built with shaders"))(*vao);
                    // A vertex array remembers its element buffer, so a stale
                    // binding from the game would be picked up by our draw.
                    if let Some(f) = ext.bind_buffer {
                        f(GL_ARRAY_BUFFER, 0);
                    }
                    (ext.active_texture.expect("built with shaders"))(GL_TEXTURE0);
                    // Without this the game's sampler object wins over the
                    // badge texture's own parameters. Minecraft leaves one bound
                    // whose min-filter wants mipmaps, which makes our
                    // single-level texture incomplete — and an incomplete
                    // texture samples as opaque black, which is precisely the
                    // black rectangle that appeared instead of the counter.
                    if let Some(f) = ext.bind_sampler {
                        f(0, 0);
                    }
                    glBindTexture(GL_TEXTURE_2D, texture);
                    (ext.uniform1i.expect("built with shaders"))(*tex, 0);
                    (ext.uniform4f.expect("built with shaders"))(*u_rect, rect[0], rect[1], rect[2], rect[3]);
                    (ext.uniform4f.expect("built with shaders"))(*tint, 1.0, 1.0, 1.0, opacity);
                    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
                }
                Painter::Immediate => {
                    glEnable(GL_TEXTURE_2D);
                    glBindTexture(GL_TEXTURE_2D, texture);
                    glMatrixMode(GL_PROJECTION);
                    glPushMatrix();
                    glLoadIdentity();
                    glMatrixMode(GL_MODELVIEW);
                    glPushMatrix();
                    glLoadIdentity();
                    glColor4f(1.0, 1.0, 1.0, opacity);
                    let (x0, y0) = (rect[0], rect[1]);
                    let (x1, y1) = (rect[0] + rect[2], rect[1] + rect[3]);
                    glBegin(GL_TRIANGLE_STRIP);
                    glTexCoord2f(0.0, 0.0);
                    glVertex2f(x0, y0);
                    glTexCoord2f(1.0, 0.0);
                    glVertex2f(x1, y0);
                    glTexCoord2f(0.0, 1.0);
                    glVertex2f(x0, y1);
                    glTexCoord2f(1.0, 1.0);
                    glVertex2f(x1, y1);
                    glEnd();
                    glMatrixMode(GL_MODELVIEW);
                    glPopMatrix();
                    glMatrixMode(GL_PROJECTION);
                    glPopMatrix();
                }
            }
            saved.restore(ext);
        }
    }

    pub fn destroy(&self, ext: &Ext) {
        if let Painter::Shader { program, vao, .. } = self {
            // SAFETY: names this object created, on the context that owns them.
            unsafe {
                if let Some(d) = ext.delete_vertex_arrays {
                    d(1, vao);
                }
                if let Some(d) = ext.delete_program {
                    d(*program);
                }
            }
        }
    }
}

/// Everything [`Painter::paint`] touches, so the game gets it all back.
struct State {
    viewport: [i32; 4],
    blend: bool,
    blend_src: i32,
    blend_dst: i32,
    depth: bool,
    cull: bool,
    scissor: bool,
    texture_2d_enabled: bool,
    /// Unit 0's binding, read *after* switching to unit 0 — see
    /// [`State::capture`].
    texture_binding: i32,
    active_texture: i32,
    /// Whether the fixed-function fields above were captured at all.
    fixed_function: bool,
    program: i32,
    vao: i32,
    array_buffer: i32,
    draw_fbo: i32,
    read_fbo: i32,
    sampler: i32,
    stencil: bool,
    /// A game that left any channel masked off would otherwise have the badge
    /// drawn through that mask — colour writes disabled is another way to end
    /// up with a black rectangle.
    color_mask: [u8; 4],
}

impl State {
    /// # Safety
    /// Current context, render thread.
    unsafe fn capture(ext: &Ext, fixed_function: bool) -> Self {
        unsafe {
            let mut viewport = [0i32; 4];
            glGetIntegerv(GL_VIEWPORT, viewport.as_mut_ptr());
            let get = |p: u32| {
                let mut v = 0i32;
                glGetIntegerv(p, &mut v);
                v
            };
            // Switch to unit 0 *before* reading the texture and sampler
            // bindings, because both queries report the **active** unit. Reading
            // them first (on whichever unit the game left active) and putting
            // them back later onto unit 0 would move one unit's texture onto
            // another and lose unit 0's original binding entirely.
            let active_texture = if ext.active_texture.is_some() { get(GL_ACTIVE_TEXTURE) } else { 0 };
            if let Some(f) = ext.active_texture {
                f(GL_TEXTURE0);
            }
            Self {
                viewport,
                blend: glIsEnabled(GL_BLEND) != 0,
                blend_src: get(GL_BLEND_SRC_RGB),
                blend_dst: get(GL_BLEND_DST_RGB),
                depth: glIsEnabled(GL_DEPTH_TEST) != 0,
                cull: glIsEnabled(GL_CULL_FACE) != 0,
                scissor: glIsEnabled(GL_SCISSOR_TEST) != 0,
                // Only a capability on a compatibility profile; asking a core
                // context about it is an error, so the immediate-mode path is
                // the only one that does.
                texture_2d_enabled: fixed_function && glIsEnabled(GL_TEXTURE_2D) != 0,
                texture_binding: get(GL_TEXTURE_BINDING_2D),
                active_texture,
                fixed_function,
                program: if ext.use_program.is_some() { get(GL_CURRENT_PROGRAM) } else { 0 },
                vao: if ext.bind_vertex_array.is_some() { get(GL_VERTEX_ARRAY_BINDING) } else { 0 },
                array_buffer: if ext.bind_buffer.is_some() { get(GL_ARRAY_BUFFER_BINDING) } else { 0 },
                draw_fbo: if ext.bind_framebuffer.is_some() { get(GL_DRAW_FRAMEBUFFER_BINDING) } else { 0 },
                read_fbo: if ext.bind_framebuffer.is_some() { get(GL_READ_FRAMEBUFFER_BINDING) } else { 0 },
                sampler: if ext.bind_sampler.is_some() { get(GL_SAMPLER_BINDING) } else { 0 },
                stencil: glIsEnabled(GL_STENCIL_TEST) != 0,
                color_mask: {
                    let mut m = [0u8; 4];
                    glGetBooleanv(GL_COLOR_WRITEMASK, m.as_mut_ptr());
                    m
                },
            }
        }
    }

    /// # Safety
    /// Same context [`capture`](Self::capture) read.
    unsafe fn restore(self, ext: &Ext) {
        unsafe {
            let toggle = |cap: u32, on: bool| {
                if on {
                    glEnable(cap)
                } else {
                    glDisable(cap)
                }
            };
            if let Some(f) = ext.bind_vertex_array {
                f(self.vao as u32);
            }
            if let Some(f) = ext.bind_buffer {
                f(GL_ARRAY_BUFFER, self.array_buffer as u32);
            }
            if let Some(f) = ext.use_program {
                f(self.program as u32);
            }
            // Still on unit 0 here, which is the unit these were read from.
            glBindTexture(GL_TEXTURE_2D, self.texture_binding as u32);
            if let Some(f) = ext.bind_sampler {
                f(0, self.sampler as u32);
            }
            // Only now is it safe to hand the active unit back.
            if let Some(f) = ext.active_texture {
                f(self.active_texture as u32);
            }
            if let Some(f) = ext.bind_framebuffer {
                f(GL_DRAW_FRAMEBUFFER, self.draw_fbo as u32);
                f(GL_READ_FRAMEBUFFER, self.read_fbo as u32);
            }
            glBlendFunc(self.blend_src as u32, self.blend_dst as u32);
            toggle(GL_BLEND, self.blend);
            toggle(GL_DEPTH_TEST, self.depth);
            toggle(GL_CULL_FACE, self.cull);
            toggle(GL_SCISSOR_TEST, self.scissor);
            toggle(GL_STENCIL_TEST, self.stencil);
            if self.fixed_function {
                toggle(GL_TEXTURE_2D, self.texture_2d_enabled);
            }
            let m = self.color_mask;
            glColorMask(m[0], m[1], m[2], m[3]);
            glViewport(self.viewport[0], self.viewport[1], self.viewport[2], self.viewport[3]);
        }
    }
}

/// Compiles one shader stage, reporting the driver's own message on failure.
///
/// # Safety
/// Current context, and `ext` must have the shader entry points.
unsafe fn compile(ext: &Ext, stage: u32, source: &[u8]) -> Result<u32, String> {
    unsafe {
        let shader = (ext.create_shader.expect("checked"))(stage);
        if shader == 0 {
            return Err("glCreateShader returned 0".into());
        }
        let ptr = source.as_ptr();
        let len = (source.len() - 1) as i32; // without the NUL
        (ext.shader_source.expect("checked"))(shader, 1, &ptr, &len);
        (ext.compile_shader.expect("checked"))(shader);

        let mut ok = 1i32;
        if let Some(g) = ext.get_shaderiv {
            g(shader, GL_COMPILE_STATUS, &mut ok);
        }
        if ok != 0 {
            return Ok(shader);
        }
        let mut message = String::from("no log");
        if let (Some(g), Some(l)) = (ext.get_shaderiv, ext.get_shader_info_log) {
            let mut len = 0i32;
            g(shader, GL_INFO_LOG_LENGTH, &mut len);
            if len > 0 {
                let mut buf = vec![0u8; len as usize];
                let mut written = 0i32;
                l(shader, len, &mut written, buf.as_mut_ptr());
                buf.truncate(written.max(0) as usize);
                message = String::from_utf8_lossy(&buf).into_owned();
            }
        }
        if let Some(d) = ext.delete_shader {
            d(shader);
        }
        Err(format!("the overlay shader did not compile: {message}"))
    }
}
