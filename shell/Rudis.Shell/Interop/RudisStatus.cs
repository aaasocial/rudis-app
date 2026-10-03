namespace Rudis.Shell.Interop;

/// <summary>
/// Transport-fault status, transcribed from the committed header
/// (crates/ffi/include/rudis_ffi.h — <c>enum RudisStatus</c>, i32 repr). Domain
/// errors NEVER appear here: they stay inside the JSON envelope as
/// <c>{"Err": ".."}</c> with status <see cref="Ok"/> — the ABI's two-layer error
/// design (D-06 / RESEARCH §2). <see cref="PanicCaught"/> (-99) follows the
/// Phase-44 RC_PANIC convention: a caught engine panic, never an unwind across
/// the boundary (FFI-02).
///
/// <para>The <c>-5..=-9</c> block (Phase 51, SHELL-04) belongs to the four
/// panel exports, which carry NO envelope — a bare status is their only
/// channel, so their real failure modes are named here rather than folded into
/// <see cref="InvalidHandle"/>.</para>
///
/// <para>⚠ APPEND ONLY, and the values are compared raw: renumbering a member
/// would compile on both sides and silently mis-diagnose every fault.
/// <c>InteropTests.status_vocabulary_mirrors_the_rust_enum</c> pins every value
/// and the member COUNT against the Rust enum.</para>
/// </summary>
internal enum RudisStatus
{
    Ok = 0,
    NullPointer = -1,
    InvalidUtf8 = -2,
    AllocationFailed = -3,
    InvalidHandle = -4,

    /// <summary>
    /// The COM pointer passed to <c>rudis_preview_attach_panel</c> is not an
    /// <c>ISwapChainPanelNative</c>: Rust's own <c>QueryInterface</c> returned
    /// <c>E_NOINTERFACE</c> (the HRESULT is on stderr). Rust QIs rather than
    /// trusting the pointer precisely so this is an error and not undefined
    /// behaviour — pass the panel itself, e.g. what
    /// <c>MarshalInspectable&lt;object&gt;.FromManaged(panel)</c> yields.
    /// </summary>
    NotASwapChainPanel = -5,

    /// <summary>
    /// A panel-affine call (attach, detach) arrived from a thread other than
    /// the one that attached. <c>ISwapChainPanelNative::SetSwapChain</c>
    /// returns <c>RPC_E_WRONG_THREAD</c> off the panel's own UI thread; attach
    /// from the panel's <c>Loaded</c> handler and detach from <c>Unloaded</c>,
    /// never from a background interop worker.
    /// </summary>
    WrongThread = -6,

    /// <summary>
    /// Surface, adapter or device creation — or the first
    /// <c>Surface::configure</c> — failed inside the engine. The failing stage
    /// and its HRESULT / wgpu error text are on stderr.
    /// </summary>
    SurfaceCreateFailed = -7,

    /// <summary>
    /// A resize, detach or content-rect call arrived with no panel attached
    /// (never attached, or already detached). Detach is idempotent-safe: the
    /// second call answers this rather than faulting.
    /// </summary>
    NotAttached = -8,

    /// <summary>
    /// <c>rudis_preview_attach_panel</c> was called while a panel is already
    /// attached. Detach first — re-attaching over a live surface is not a
    /// supported transition.
    /// </summary>
    AlreadyAttached = -9,

    PanicCaught = -99,
}
