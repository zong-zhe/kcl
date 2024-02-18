use std::fs;

use crate::*;

#[no_mangle]
#[runtime_fn]
pub extern "C" fn kclvm_file_read(
    ctx: *mut kclvm_context_t,
    args: *const kclvm_value_ref_t,
    _kwargs: *const kclvm_value_ref_t,
) -> *const kclvm_value_ref_t {
    let args = ptr_as_ref(args);
    let ctx = mut_ptr_as_ref(ctx);

    if let Some(x) = args.arg_i_str(0, None) {
        let contents =
            fs::read_to_string(&x).expect(&format!("failed to access the file in {}", x));

        let s = ValueRef::str(contents.as_ref());
        return s.into_raw(ctx);
    }

    panic!("read() takes exactly one argument (0 given)");
}

#[no_mangle]
#[runtime_fn]
pub extern "C" fn kclvm_file_abs(
    ctx: *mut kclvm_context_t,
    args: *const kclvm_value_ref_t,
    _kwargs: *const kclvm_value_ref_t,
) -> *const kclvm_value_ref_t {
    let args = ptr_as_ref(args);
    let ctx = mut_ptr_as_ref(ctx);

    if let Some(x) = args.arg_i_str(0, None) {
        match fs::canonicalize(x.to_string()) {
            Ok(abs_path) => {
                return ValueRef::str(abs_path.to_str().expect("failed to convert path to str")).into_raw(ctx);
            },
            Err(e) => panic!("failed to access the file in {}", x),
        }
    }

    panic!("abs() takes exactly one argument (0 given)");
}