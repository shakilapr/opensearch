fn main() {
    let p = search_engine::fuse::FusionPolicy::default();
    println!("weights: {:?}", p.weights);
}
