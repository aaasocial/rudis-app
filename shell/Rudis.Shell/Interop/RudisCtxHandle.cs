using System.Runtime.InteropServices;

namespace Rudis.Shell.Interop;

/// <summary>
/// FFI-01's production managed <see cref="SafeHandle"/> over the opaque
/// <c>RudisCtx</c> — the pattern promoted from the Phase 47 harness
/// (spikes/47-ffi-harness — technique reference; the harness never ships). The
/// runtime guarantees <see cref="ReleaseHandle"/> runs AT MOST ONCE, even under
/// races and finalization — the double-free / use-after-free mitigation the
/// header's <c>rudis_shutdown</c> doc names (T-47-02/T-47-04; T-50-11). The raw
/// pointer never leaves this type: every ctx-taking export is declared against
/// <see cref="RudisCtxHandle"/>, and the marshaller's AddRef/Release brackets keep
/// the handle live across each native call.
/// </summary>
internal sealed class RudisCtxHandle : SafeHandle
{
    public RudisCtxHandle() : base(nint.Zero, ownsHandle: true) { }

    /// <summary>Null = <c>rudis_init</c> failed — a DESIGNED outcome for malformed
    /// config JSON (crates/ffi/src/lib.rs:191-201). Callers null-check, never
    /// assume (T-50-13).</summary>
    public override bool IsInvalid => handle == nint.Zero;

    /// <summary>
    /// <c>rudis_shutdown</c> is teardown-only — it persists nothing, exactly like
    /// the retired Tauri shell's close path did (the parity close contract, 50-02 §2.3).
    /// <see cref="RudisNative.Dispose"/> drains and joins the interop worker BEFORE
    /// handle disposal reaches this, so release can never race an in-flight
    /// command (50-02 §1.5(5)).
    /// </summary>
    protected override bool ReleaseHandle()
        => RudisNative.Shutdown(handle) == RudisStatus.Ok;
}
