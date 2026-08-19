use tiamot_core::script::ScriptVm;

fn main() {
    let mut vm =
        tiamot_core::script::MluaVm::new(tiamot_core::script::VmLimits::default()).expect("vm");
    let dir = std::path::Path::new("game/core_blocks");
    let src = std::fs::read_to_string(dir.join("init.lua")).expect("read");
    vm.load_mod("core_blocks", &src, dir).expect("load");
    for rules in vm.registered_block_rules() {
        println!("{} step={:?}", rules.block, rules.step_sound);
    }
}
