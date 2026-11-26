#![cfg(feature = "web-fetch")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::{StreamExt, stream};
use jimu_agent::{AgentTool, AgentToolResult};
use jimu_ai::{
    BoxFuture, CancelToken, HttpRequest, HttpStreamResponse, HttpTransport, TransportError,
};
use jimu_tools::web::web2md::{
    ParseContext, ParserRule, Web2Md, Web2MdDocument, Web2MdError, Web2MdParser,
};
use jimu_tools::{WebFetchConfig, WebFetchTool};
use serde_json::{Value, json};

#[derive(Default)]
struct Replay {
    responses: HashMap<String, Reply>,
    requests: Mutex<Vec<String>>,
}

enum Reply {
    Body(&'static str, String),
    Redirect(u16, &'static str),
    Oversize,
    Bytes(&'static str, Vec<u8>, usize),
}

impl Replay {
    fn new(entries: impl IntoIterator<Item = (&'static str, Reply)>) -> Self {
        Self {
            responses: entries
                .into_iter()
                .map(|(url, reply)| (url.into(), reply))
                .collect(),
            requests: Mutex::default(),
        }
    }
}

impl HttpTransport for Replay {
    fn send(
        &self,
        request: HttpRequest,
        _: CancelToken,
    ) -> BoxFuture<'_, Result<HttpStreamResponse, TransportError>> {
        Box::pin(async move {
            assert_eq!(request.method, "GET");
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(name, _)| name == "x-no-redirect")
            );
            assert!(request.headers.iter().all(|(name, _)| !matches!(
                name.as_str(),
                "authorization" | "cookie" | "x-api-key"
            )));
            if request.url.starts_with("https://api.fxtwitter.com/") {
                let reader = (
                    "user-agent".into(),
                    "Mozilla/5.0 (compatible; Web2MD/1.0)".into(),
                );
                assert!(request.headers.contains(&reader));
            }
            let url = request.url;
            self.requests.lock().unwrap().push(url.clone());
            let response = match self.responses.get(&url) {
                Some(Reply::Body(mime, body)) => HttpStreamResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), (*mime).into())],
                    body: Box::pin(stream::iter([Ok(body.clone().into())])),
                },
                Some(Reply::Bytes(mime, bytes, length)) => HttpStreamResponse {
                    status: 200,
                    headers: vec![
                        ("Content-Type".into(), (*mime).into()),
                        ("Content-Length".into(), length.to_string()),
                    ],
                    body: Box::pin(stream::iter([Ok(bytes.clone().into())])),
                },
                Some(Reply::Redirect(status, location)) => {
                    return Err(TransportError::HttpStatus {
                        status: *status,
                        headers: vec![("Location".into(), (*location).into())],
                        body: String::new(),
                    });
                }
                Some(Reply::Oversize) => HttpStreamResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/plain".into())],
                    body: Box::pin(stream::iter([Ok("12345".into()), Ok("6789".into())]).chain(
                        stream::poll_fn(|_| panic!("oversize body was still being downloaded")),
                    )),
                },
                None => {
                    return Err(TransportError::HttpStatus {
                        status: 404,
                        headers: vec![],
                        body: String::new(),
                    });
                }
            };
            Ok(response)
        })
    }
}

fn config() -> WebFetchConfig {
    WebFetchConfig {
        host_resolver: Some(Arc::new(|host| {
            Ok(if host == "private.example" {
                vec![
                    "93.184.216.34".parse().unwrap(),
                    "192.168.0.2".parse().unwrap(),
                ]
            } else {
                vec!["93.184.216.34".parse().unwrap()]
            })
        })),
        ..Default::default()
    }
}

async fn fetch(tool: &WebFetchTool, args: Value) -> Result<AgentToolResult, String> {
    tool.execute("fetch".into(), args, CancelToken::new(), Arc::new(|_| {}))
        .await
}

fn text(result: &AgentToolResult) -> &str {
    let jimu_ai::ContentPart::Text(part) = &result.content[0] else {
        panic!("expected text")
    };
    &part.text
}

struct PageParser;

impl Web2MdParser for PageParser {
    fn name(&self) -> &str {
        "custom"
    }

    fn parse<'a>(
        &'a self,
        context: &'a mut ParseContext,
    ) -> BoxFuture<'a, Result<Option<Web2MdDocument>, Web2MdError>> {
        Box::pin(async move {
            let dom = context.document().await?;
            Ok(dom.query("article.custom p").map(|element| {
                Web2MdDocument::new(
                    dom.url.clone(),
                    element.text().collect::<String>(),
                    self.name(),
                )
            }))
        })
    }
}

#[tokio::test]
async fn extracts_html_after_redirect_and_shares_the_page_with_custom_rules() {
    let html = r##"<!doctype html><html lang="en"><head>
      <base href="/assets/"><title>Example Article | Jimu</title><meta property="og:site_name" content="Jimu">
      <meta property="og:title" content="Example Article | Jimu">
      <meta property="article:published_time" content="2026-11-21T12:00:00Z">
      <meta name="author" content="Ada"><meta name="description" content="Description">
      <meta property="og:image" content="cover.png"></head><body>
      <header class="site-header">Site menu</header><nav>Navigation links</nav>
      <main><article><h1>Example Article</h1><p>Intro <strong> bold </strong>tail.
        See the <a href="docs/readme">manual</a> and <a href="javascript:void(0)">unsafe URL</a>.</p>
        <h2>Code</h2><pre><code class="language-rust">let x = 1;
  println!("{x}");</code></pre>
        <ul><li>Parent<ul><li>Child</li></ul></li></ul>
        <table><tr><th>Name</th><th>Value</th></tr><tr><td>Milk</td><td>2 | 3</td></tr></table>
        <p>Inline <span class="katex"><span aria-hidden="true">duplicate math</span>
          <math><semantics><mi>x</mi><annotation encoding="application/x-tex">x^2</annotation></semantics></math></span>.</p>
        <math display="block"><mfrac><mn>1</mn><mi>x</mi></mfrac></math>
        <aside class="admonition warning callout"><p>Try a new key.</p></aside>
        <img data-src="diagram.png" alt="diagram">
        <p>Referenced text<sup><a href="#fn-1">1</a></sup>.</p>
        <div style="DISPLAY: none !important">Hidden spam</div><div class="cookie-banner">Accept cookies</div>
        <aside>Sidebar backup</aside><div id="share-links">Share this post</div></article></main>
        <section class="footnotes"><ol><li id="fn-1">Footnote text <a class="footnote-backref" href="#back">back</a></li></ol></section>
        <footer>Copyright boilerplate</footer></body></html>"##;
    let start = "https://first.example/go";
    let final_url = "https://other.example/post";
    let custom = "https://other.example/custom";
    let replay = Arc::new(Replay::new([
        (start, Reply::Redirect(302, final_url)),
        (
            final_url,
            Reply::Body("Text/HTML; charset=utf-8", html.into()),
        ),
        (
            custom,
            Reply::Body(
                "application/xhtml+xml",
                "<article class=custom><p>Custom content.</p></article>".into(),
            ),
        ),
    ]));
    let mut engine = Web2Md::new(replay.clone(), config());
    engine.register(ParserRule::for_page(
        |dom| dom.query("article.custom").is_some(),
        PageParser,
    ));
    let document = engine
        .convert_html(html, final_url, CancelToken::new())
        .await
        .unwrap();
    assert_eq!(
        (
            document.site.as_str(),
            document.language.as_str(),
            document.description.as_str()
        ),
        ("Jimu", "en", "Description")
    );
    assert_eq!(document.image, "https://other.example/assets/cover.png");

    let tool = WebFetchTool::with_web2md(engine);
    assert_eq!(tool.definition().name, "web_fetch");
    let result = fetch(&tool, json!({"url":start})).await.unwrap();
    assert_eq!(result.details["final_url"], final_url);
    assert_eq!(result.details["parser"], "generic");
    assert_eq!(result.details["content_type"], "text/html");
    let md = text(&result);
    for expected in [
        "Title: Example Article",
        "Author: Ada",
        "Published: 2026-11-21T12:00:00Z",
        "Intro **bold** tail",
        "[manual](https://other.example/assets/docs/readme)",
        "## Code",
        "```rust\nlet x = 1;\n  println!(\"{x}\");\n```",
        "- Parent\n  - Child",
        "| Name | Value |\n| --- | --- |\n| Milk | 2 \\| 3 |",
        "Inline $x^2$",
        "$$\n\\frac{1}{x}\n$$",
        "> [!WARNING]\n> Try a new key.",
        "![diagram](https://other.example/assets/diagram.png)",
        "text[^1]",
        "[^1]: Footnote text",
    ] {
        assert!(md.contains(expected), "missing {expected:?} in\n{md}");
    }
    for clutter in [
        "Navigation links",
        "Hidden spam",
        "Accept cookies",
        "Sidebar backup",
        "Share this post",
        "Copyright boilerplate",
        "duplicate math",
        "javascript:",
        "# Example Article",
    ] {
        assert!(!md.contains(clutter), "found {clutter:?} in\n{md}");
    }
    let result = fetch(&tool, json!({"url":custom})).await.unwrap();
    assert_eq!(result.details["parser"], "custom");
    assert!(text(&result).ends_with("Custom content."));
    assert_eq!(*replay.requests.lock().unwrap(), [start, final_url, custom]);
    assert_eq!(
        text(
            &fetch(&tool, json!({"url":final_url,"raw":true}))
                .await
                .unwrap()
        ),
        html
    );
}

#[tokio::test]
async fn x_post_and_article_use_api_replay_without_downloading_html() {
    let status = "https://api.fxtwitter.com/2/status/20";
    let article = "https://api.fxtwitter.com/2/thread/1234567890";
    let fixture: Value = std::env::var_os("JIMU_X_ARTICLE_FIXTURE").map_or_else(
        || serde_json::from_str(X_ARTICLE_FIXTURE).unwrap(),
        |path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap(),
    );
    let mut quoted = fixture[status].clone();
    let api_thread = "https://api.fxtwitter.com/2/thread/20";
    quoted["status"]["quote"] =
        json!({"id":"2","text":"Quoted post","author":{"name":"A","screen_name":"a"}});
    quoted["status"]["media"]["all"] = json!([{"type":"photo","url":"https://pbs.twimg.com/media/post.jpg","altText":"Post photo"}]);
    let replay = Arc::new(Replay::new([
        (
            api_thread,
            Reply::Body("application/json", quoted.to_string()),
        ),
        (
            article,
            Reply::Body("application/json", fixture[article].to_string()),
        ),
        (
            status,
            Reply::Body("application/json", fixture[status].to_string()),
        ),
    ]));
    let tool = WebFetchTool::new(replay.clone(), config());
    let post = fetch(&tool, json!({"url":"https://twitter.com/jack/status/20"}))
        .await
        .unwrap();
    assert_eq!(post.details["parser"], "x");
    assert!(text(&post).contains("![Post photo](https://pbs.twimg.com/media/post.jpg)"));
    assert!(text(&post).contains("> Quoted post"));

    let result = fetch(
        &tool,
        json!({"url":"https://x.com/tester/article/1234567890"}),
    )
    .await
    .unwrap();
    assert_eq!(result.details["title"], "All Formats Article");
    assert_eq!(
        result.details["final_url"],
        "https://x.com/tester/status/1234567890"
    );
    let (header, markdown) = text(&result).split_once("\n\n").unwrap();
    assert!(header.contains("Published: 2026-03-23T14:08:00Z"));
    assert_eq!(markdown, SWIFT_ARTICLE_MARKDOWN);
    assert!(
        replay
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|url| url.starts_with("https://api.fxtwitter.com/"))
    );
}

#[tokio::test]
async fn returns_text_and_images_with_limits_and_blocks_private_redirects() {
    let json_url = "https://public.example/json";
    let plain = "https://public.example/plain";
    let image = "https://public.example/image";
    let large = "https://public.example/large";
    let redirect = "https://public.example/private-redirect";
    let local_redirect = "https://public.example/local-redirect";
    let loop_url = "https://public.example/loop";
    let private = "http://169.254.169.254/metadata";
    let replay = Arc::new(Replay::new([
        (
            json_url,
            Reply::Body("application/json", r#"{"a":1}"#.into()),
        ),
        (plain, Reply::Body("text/plain", "😄é".into())),
        (image, Reply::Body("image/png", "abc".into())),
        (large, Reply::Oversize),
        (redirect, Reply::Redirect(307, private)),
        (
            local_redirect,
            Reply::Redirect(302, "http://127.0.0.1:8080/"),
        ),
        (loop_url, Reply::Redirect(308, "/loop")),
    ]));
    let tool = WebFetchTool::new(
        replay.clone(),
        WebFetchConfig {
            max_download_bytes: 8,
            ..config()
        },
    );
    assert_eq!(
        text(&fetch(&tool, json!({"url":json_url})).await.unwrap()),
        r#"{"a":1}"#
    );
    let result = fetch(&tool, json!({"url":plain,"max_bytes":4}))
        .await
        .unwrap();
    assert_eq!(text(&result), "😄");
    assert_eq!(result.details["truncated"], true);
    assert!(
        fetch(&tool, json!({"url":image,"max_bytes":0}))
            .await
            .unwrap_err()
            .contains("Unsupported image")
    );
    assert_eq!(
        fetch(&tool, json!({"url":large})).await.unwrap_err(),
        "The response is too large"
    );

    for url in [
        "http://localhost./",
        "http://abc.localhost/",
        "http://a.local/",
        "http://127.20.0.1/",
        "http://0.1.2.3/",
        "http://10.0.0.1/",
        "http://172.16.0.1/",
        "http://192.168.0.1/",
        "http://100.65.0.1/",
        private,
        "http://[::1]/",
        "http://[fd00::1]/",
        "http://[fe80::1]/",
        "http://[3fff::1]/",
        "http://[::ffff:127.0.0.1]/",
        "https://private.example/",
        redirect,
        local_redirect,
    ] {
        assert_eq!(
            fetch(&tool, json!({"url":url})).await.unwrap_err(),
            "This address is not public",
            "{url}"
        );
    }
    for url in [
        "https://name:password@public.example/",
        "file:///etc/passwd",
    ] {
        assert!(
            fetch(&tool, json!({"url":url}))
                .await
                .unwrap_err()
                .contains("valid HTTP")
        );
    }
    assert_eq!(
        fetch(&tool, json!({"url":loop_url})).await.unwrap_err(),
        "The website redirected too many times"
    );
    for (url, scope) in [(local_redirect, redirect), (private, private)] {
        let origin = url::Url::parse(scope).unwrap().origin();
        let scoped_tool = tool.clone().with_private_origin(origin);
        assert_eq!(
            fetch(&scoped_tool, json!({"url":url})).await.unwrap_err(),
            "This address is not public"
        );
    }
    let requests = replay.requests.lock().unwrap();
    assert_eq!(
        &requests[..5],
        [
            json_url,
            plain,
            "https://public.example/image",
            large,
            redirect
        ]
    );
    assert_eq!(requests[5], local_redirect);
    assert_eq!(&requests[6..12], [loop_url; 6]);
    assert_eq!(&requests[12..], [local_redirect]);
}

// Article/status bodies from web2md-swift Tools/fixtures/x-article-formats.json (MIT).
// The article's thread array repeats the main status and is omitted here.
const X_ARTICLE_FIXTURE: &str = r###"{
  "https://api.fxtwitter.com/2/thread/1234567890": {
    "code": 200,
    "status": {
      "type": "status",
      "url": "https://x.com/tester/status/1234567890",
      "id": "1234567890",
      "text": "Teaser https://x.com/i/article/555",
      "author": {
        "screen_name": "tester",
        "name": "Test Author",
        "id": "1"
      },
      "created_at": "Mon Mar 23 14:08:00 +0000 2026",
      "created_timestamp": 1774274880,
      "lang": "en",
      "media": {},
      "article": {
        "id": "555",
        "title": "All Formats Article",
        "preview_text": "Preview text",
        "created_at": "2026-03-23T14:08:00.000Z",
        "cover_media": {
          "media_info": {
            "__typename": "ApiImage",
            "original_img_url": "https://pbs.twimg.com/media/cover.jpg",
            "original_img_width": 1600,
            "original_img_height": 900
          }
        },
        "content": {
          "blocks": [
            {"key":"a","type":"unstyled","text":"Intro with bold, italic, strike and both overlapping words here.","depth":0,"inlineStyleRanges":[{"offset":11,"length":4,"style":"Bold"},{"offset":17,"length":6,"style":"Italic"},{"offset":25,"length":6,"style":"Strikethrough"},{"offset":41,"length":11,"style":"Bold"},{"offset":46,"length":6,"style":"Italic"}],"entityRanges":[],"data":{}},
            {"key":"b","type":"header-one","text":"Section heading","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"c","type":"unstyled","text":"🚀 Launch day is 今天 with emoji 🎉 bold after.","depth":0,"inlineStyleRanges":[{"offset":2,"length":6,"style":"Bold"},{"offset":32,"length":4,"style":"Bold"}],"entityRanges":[],"data":{}},
            {"key":"d","type":"unstyled","text":"Read the docs, ping @jack, see #swift and $TSLA. Plain snake_case and *stars* stay literal.","depth":0,"inlineStyleRanges":[],"entityRanges":[{"key":0,"offset":0,"length":4}],"data":{"urls":[{"fromIndex":9,"toIndex":13,"text":"https://example.com/docs"}],"mentions":[{"fromIndex":20,"toIndex":25,"text":"jack"}],"hashtags":[{"fromIndex":31,"toIndex":37,"text":"swift"}],"cashtags":[{"fromIndex":42,"toIndex":47,"text":"TSLA"}]}},
            {"key":"e","type":"header-two","text":"Sub heading","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"f","type":"ordered-list-item","text":"First step","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"g","type":"ordered-list-item","text":"Second step with nested bullets","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"h","type":"unordered-list-item","text":"nested bullet","depth":1,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"i","type":"unordered-list-item","text":"deeper bullet","depth":2,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"j","type":"ordered-list-item","text":"Third step","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"k","type":"unordered-list-item","text":"Separate bullet list","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"l","type":"blockquote","text":"A quote line one\nand a soft break.","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"m","type":"blockquote","text":"Second quote paragraph.","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"n","type":"atomic","text":" ","depth":0,"inlineStyleRanges":[],"entityRanges":[{"key":1,"offset":0,"length":1}],"data":{}},
            {"key":"o","type":"atomic","text":" ","depth":0,"inlineStyleRanges":[],"entityRanges":[{"key":2,"offset":0,"length":1}],"data":{}},
            {"key":"p","type":"atomic","text":" ","depth":0,"inlineStyleRanges":[],"entityRanges":[{"key":3,"offset":0,"length":1}],"data":{}},
            {"key":"q","type":"atomic","text":" ","depth":0,"inlineStyleRanges":[],"entityRanges":[{"key":5,"offset":0,"length":1}],"data":{}},
            {"key":"r","type":"code-block","text":"let x = 1","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"s","type":"code-block","text":"print(x)","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"t","type":"atomic","text":" ","depth":0,"inlineStyleRanges":[],"entityRanges":[{"key":6,"offset":0,"length":1}],"data":{}},
            {"key":"u","type":"atomic","text":" ","depth":0,"inlineStyleRanges":[],"entityRanges":[{"key":7,"offset":0,"length":1}],"data":{}},
            {"key":"v","type":"unstyled","text":"","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"w","type":"unstyled","text":"1. Not a list and # not a heading","depth":0,"inlineStyleRanges":[],"entityRanges":[],"data":{}},
            {"key":"x","type":"unstyled","text":"Ends with inline code","depth":0,"inlineStyleRanges":[{"offset":17,"length":4,"style":"CODE"}],"entityRanges":[],"data":{}}
          ],
          "entityMap": [
            {"key":"0","value":{"type":"LINK","mutability":"Mutable","data":{"url":"https://example.com/read"}}},
            {"key":"1","value":{"type":"MARKDOWN","mutability":"Mutable","data":{"entityKey":"1","markdown":"```swift\nlet greeting = \"hi\"\n```"}}},
            {"key":"2","value":{"type":"MEDIA","mutability":"Immutable","data":{"caption":"A photo caption","mediaItems":[{"localMediaId":"1","mediaCategory":"DraftTweetImage","mediaId":"m1"}]}}},
            {"key":"3","value":{"type":"MEDIA","mutability":"Immutable","data":{"mediaItems":[{"localMediaId":"2","mediaCategory":"AmplifyVideo","mediaId":"v1"}]}}},
            {"key":"5","value":{"type":"TWEET","mutability":"Immutable","data":{"tweetId":"20"}}},
            {"key":"6","value":{"type":"DIVIDER","mutability":"Immutable","data":{}}},
            {"key":"7","value":{"type":"TWEET","mutability":"Immutable","data":{"tweetId":"999"}}}
          ]
        },
        "media_entities": [
          {"media_id":"m1","media_info":{"__typename":"ApiImage","original_img_url":"https://pbs.twimg.com/media/photo1.jpg","original_img_width":1200,"original_img_height":800}},
          {"media_id":"v1","media_info":{"__typename":"ApiVideo","preview_image":{"original_img_url":"https://pbs.twimg.com/amplify_video_thumb/v1.jpg"},"variants":[{"content_type":"application/x-mpegURL","url":"https://video.twimg.com/v1.m3u8"},{"bit_rate":832000,"content_type":"video/mp4","url":"https://video.twimg.com/v1_low.mp4"},{"bit_rate":2176000,"content_type":"video/mp4","url":"https://video.twimg.com/v1_high.mp4"}]}}
        ]
      }
    },
    "author": {
      "screen_name": "tester",
      "name": "Test Author",
      "id": "1"
    }
  },
  "https://api.fxtwitter.com/2/status/20": {
    "code": 200,
    "status": {
      "type": "status",
      "url": "https://x.com/jack/status/20",
      "id": "20",
      "text": "just setting up my twttr",
      "raw_text": {
        "text": "just setting up my twttr",
        "facets": []
      },
      "author": {
        "screen_name": "jack",
        "name": "jack",
        "id": "12"
      },
      "media": {}
    }
  }
}"###;

// Output of the Swift CLI replaying the same source fixture.
const SWIFT_ARTICLE_MARKDOWN: &str = concat!(
    r###"![](https://pbs.twimg.com/media/cover.jpg)

Intro with **bold**, *italic*, ~~strike~~ and both **overl*apping*** words here.

## Section heading

🚀 **Launch** day is 今天 with emoji 🎉 **bold** after.

[Read](https://example.com/read) the [docs](https://example.com/docs), ping [@jack](https://x.com/jack), see [#swift](https://x.com/hashtag/swift) and [$TSLA](https://x.com/search?q=%24TSLA). Plain snake_case and \*stars\* stay literal.

### Sub heading

1. First step
2. Second step with nested bullets
   - nested bullet
     - deeper bullet
3. Third step
- Separate bullet list

> A quote line one"###,
    "  \n",
    r###"> and a soft break.
>
> Second quote paragraph.

```swift
let greeting = "hi"
```

![A photo caption](https://pbs.twimg.com/media/photo1.jpg)

*A photo caption*

[![](https://pbs.twimg.com/amplify_video_thumb/v1.jpg)](https://video.twimg.com/v1_high.mp4)

> [jack (@jack)](https://x.com/jack/status/20)
>
> just setting up my twttr

```
let x = 1
print(x)
```

---

> [https://x.com/i/status/999](https://x.com/i/status/999)

1\. Not a list and # not a heading

Ends with inline `code`"###
);

#[tokio::test]
async fn ssrf_pinning_header_and_web2md_edge_cases_work() {
    const FINAL_URL: &str = "https://docs.github.com/final";
    const LOCAL_URL: &str = "http://macbook-pro.local:3000/";

    struct HeaderCapture {
        headers: Mutex<Vec<Vec<(String, String)>>>,
        responses: HashMap<String, Reply>,
    }

    impl HttpTransport for HeaderCapture {
        fn send(
            &self,
            request: HttpRequest,
            _: CancelToken,
        ) -> BoxFuture<'_, Result<HttpStreamResponse, TransportError>> {
            Box::pin(async move {
                self.headers.lock().unwrap().push(request.headers.clone());
                match self.responses.get(&request.url) {
                    Some(Reply::Body(mime, body)) => Ok(HttpStreamResponse {
                        status: 200,
                        headers: vec![("Content-Type".into(), (*mime).into())],
                        body: Box::pin(stream::iter([Ok(body.clone().into())])),
                    }),
                    Some(Reply::Redirect(status, location)) => Err(TransportError::HttpStatus {
                        status: *status,
                        headers: vec![("Location".into(), (*location).into())],
                        body: String::new(),
                    }),
                    _ => Err(TransportError::HttpStatus {
                        status: 404,
                        headers: vec![],
                        body: String::new(),
                    }),
                }
            })
        }
    }

    // 1. Domain requests carry `x-resolve-addrs`; IP literals do not; redirects route on final URL.
    let capture = Arc::new(HeaderCapture {
        headers: Mutex::default(),
        responses: HashMap::from([
            (
                "https://short.example/go".into(),
                Reply::Redirect(302, FINAL_URL),
            ),
            (
                FINAL_URL.into(),
                Reply::Body(
                    "text/html",
                    "<html><body><article><p>Fallback</p></article></body></html>".into(),
                ),
            ),
            (
                "https://93.184.216.34/ip".into(),
                Reply::Body("text/plain", "ip-ok".into()),
            ),
            (
                LOCAL_URL.into(),
                Reply::Body("text/plain", "local-ok".into()),
            ),
        ]),
    });
    let config = WebFetchConfig {
        host_resolver: Some(Arc::new(|host| {
            let ips: &[&str] = if host == "macbook-pro.local" {
                &["fe80::1", "::1", "127.0.0.1", "192.168.1.5"]
            } else {
                &["198.18.0.18", "::ffff:198.19.0.18"]
            };
            Ok(ips.iter().map(|ip| ip.parse().unwrap()).collect())
        })),
        ..WebFetchConfig::default()
    };

    struct RedirectRuleParser;
    impl Web2MdParser for RedirectRuleParser {
        fn name(&self) -> &str {
            "redirect-final"
        }
        fn parse<'a>(
            &'a self,
            context: &'a mut ParseContext,
        ) -> BoxFuture<'a, Result<Option<Web2MdDocument>, Web2MdError>> {
            Box::pin(async move {
                Ok(Some(Web2MdDocument::new(
                    context.url.clone(),
                    format!("Matched final URL: {}", context.url),
                    self.name(),
                )))
            })
        }
    }

    let mut engine = Web2Md::new(capture.clone(), config.clone());
    engine.register(ParserRule::for_url(
        |url| url.as_str() == FINAL_URL,
        RedirectRuleParser,
    ));
    let redirected = engine
        .convert("https://short.example/go", CancelToken::new())
        .await
        .unwrap();
    assert_eq!(redirected.parser, "redirect-final");
    assert!(redirected.markdown.contains(FINAL_URL));

    let ip_tool = WebFetchTool::new(capture.clone(), config);
    fetch(&ip_tool, json!({"url": "https://93.184.216.34/ip"}))
        .await
        .unwrap();

    for url in ["http://198.18.0.18/", "http://[::ffff:198.19.0.18]/"] {
        assert_eq!(
            fetch(&ip_tool, json!({"url":url})).await.unwrap_err(),
            "This address is not public"
        );
    }

    let origin = ip_tool
        .private_origin(LOCAL_URL, &CancelToken::new())
        .await
        .unwrap()
        .unwrap();
    fetch(
        &ip_tool.with_private_origin(origin),
        json!({"url":LOCAL_URL}),
    )
    .await
    .unwrap();

    {
        let recorded = capture.headers.lock().unwrap();
        assert_eq!(recorded.len(), 4);
        let names_and_values = format!("{recorded:?}").to_ascii_lowercase();
        assert!(!names_and_values.contains("jimu"));
        let browser = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";
        let reader = "Mozilla/5.0 (compatible; Web2MD/1.0)";
        for (headers, ua) in recorded.iter().zip([browser, reader, browser]) {
            assert!(headers.contains(&("user-agent".into(), ua.into())));
        }
        let pinned = (
            "x-resolve-addrs".into(),
            "198.18.0.18:443,[::ffff:198.19.0.18]:443".into(),
        );
        assert!(
            recorded[0].contains(&pinned),
            "domain fetch must pin resolved SocketAddr: {:?}",
            recorded[0]
        );
        assert!(
            !recorded[2].iter().any(|(k, _)| k == "x-resolve-addrs"),
            "IP literal fetch must not send x-resolve-addrs: {:?}",
            recorded[2]
        );
        assert!(recorded[3].contains(&(
            "x-resolve-addrs".into(),
            "[::1]:3000,127.0.0.1:3000,192.168.1.5:3000".into(),
        )));
    }

    // 2. Offline convert_html never calls XParser over network, handles self-referential footnote,
    // preserves wrapped heading anchors, removes symbol anchors, and keeps sorted attrs on links/images.
    let offline_engine = Web2Md::new(Arc::new(Replay::new([])), WebFetchConfig::default());
    let html = r##"<!doctype html><html><head><title>Doc</title></head><body>
        <article id="fn-1" class="footnotes">
            <h2><a href="#intro">Introduction</a></h2>
            <h3>Details <a href="#details">#</a></h3>
            <p>See <a id="ref-link" class="ext" href="/guide">the guide</a> and <a href="#fn-1">note</a>.</p>
            <p><img alt="Logo" title="Brand" data-src="/img/logo.png" /></p>
        </article>
    </body></html>"##;
    let doc = offline_engine
        .convert_html(
            html,
            "https://x.com/user/status/1234567890",
            CancelToken::new(),
        )
        .await
        .unwrap();
    assert!(doc.markdown.contains("## Introduction"), "{}", doc.markdown);
    assert!(doc.markdown.contains("### Details"), "{}", doc.markdown);
    assert!(!doc.markdown.contains("### Details #"), "{}", doc.markdown);
    assert!(
        doc.markdown.contains("[the guide](https://x.com/guide)"),
        "sorted attrs must keep rewritten href: {}",
        doc.markdown
    );
    assert!(
        doc.markdown.contains("![Logo](https://x.com/img/logo.png)"),
        "sorted attrs must keep rewritten src: {}",
        doc.markdown
    );
}

#[tokio::test]
async fn fetch_checks_image_bytes_and_response_length() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use image::{DynamicImage, ImageFormat};
    let good = "https://public.example/photo";
    let damaged = "https://public.example/damaged";
    let partial = "https://public.example/partial";
    let mut out = std::io::Cursor::new(Vec::new());
    DynamicImage::new_rgb8(2, 1)
        .write_to(&mut out, ImageFormat::WebP)
        .unwrap();
    let mut bytes = out.into_inner();
    let encoded = STANDARD.encode(&bytes);
    let len = bytes.len();
    let good_reply = Reply::Bytes("text/html", bytes.clone(), len);
    bytes.pop();
    let replay = Replay::new([
        (good, good_reply),
        (damaged, Reply::Bytes("image/webp", bytes, len - 1)),
        (partial, Reply::Bytes("text/plain", b"partial".to_vec(), 20)),
    ]);
    let tool = WebFetchTool::new(Arc::new(replay), config());
    let result = fetch(&tool, json!({"url":good})).await.unwrap();
    let [jimu_ai::ContentPart::Image(image)] = result.content.as_slice() else {
        panic!("expected WebP")
    };
    assert_eq!(image.mime_type, "image/webp");
    assert_eq!(image.data, encoded);
    assert!(
        fetch(&tool, json!({"url":damaged}))
            .await
            .unwrap_err()
            .contains("incomplete")
    );
    assert_eq!(
        fetch(&tool, json!({"url":partial})).await.unwrap_err(),
        Web2MdError::Network.to_string()
    );
}
