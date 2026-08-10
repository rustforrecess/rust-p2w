//! Exactly the transcendentals the curriculum needs, exported flat so the
//! generator can lift them: pow for `**`, exp for sigmoid/softmax, log for
//! entropy and log-loss, log2/log10 for the same in other bases. sqrt is NOT
//! here — `f64.sqrt` is a WASM instruction and codegen emits it directly.
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn m_pow(a: f64, b: f64) -> f64 {
    libm::pow(a, b)
}
#[unsafe(no_mangle)]
pub extern "C" fn m_exp(x: f64) -> f64 {
    libm::exp(x)
}
#[unsafe(no_mangle)]
pub extern "C" fn m_log(x: f64) -> f64 {
    libm::log(x)
}
#[unsafe(no_mangle)]
pub extern "C" fn m_log2(x: f64) -> f64 {
    libm::log2(x)
}
#[unsafe(no_mangle)]
pub extern "C" fn m_log10(x: f64) -> f64 {
    libm::log10(x)
}
