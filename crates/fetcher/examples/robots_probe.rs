fn main() {
    let p = search_fetcher::robots::parse(
        "User-agent: *\nDisallow: /\n",
        "agent0-crawl-test/0.1 (+test)",
        0,
    );
    println!("rules: {:?}", p.rules);
    println!("status: {:?}", p.status);
    println!("(/): {:?}", search_fetcher::robots::evaluate(&p, "/"));
    println!("(/a): {:?}", search_fetcher::robots::evaluate(&p, "/a"));
}
