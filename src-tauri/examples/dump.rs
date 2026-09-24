fn main() {
    // The app loads the price table and the plan-usage windows on background
    // threads at startup; a one-shot dump has to ask for both.
    tokenscope_lib::load_pricing();
    tokenscope_lib::load_limits();
    println!("{}", tokenscope_lib::dashboard_json());
}
