use std::sync::LazyLock;

use scraper::Selector;

pub(super) static CLUTTER: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(concat!(
    "script:not([type^='math/tex']),style,noscript,template,iframe,object,embed,canvas,svg,",
    "nav,footer,form,button,dialog,aside:not([data-callout]):not([class*='admonition']):not([class*='callout']):not([role='doc-footnote']):not([class*='footnote']):not([data-type='footnote']),",
    "[hidden],.hidden,.invisible,[role='navigation'],[role='complementary'],[role='banner'],",
    "[role='contentinfo'],[role='dialog'],[role='alertdialog'],[data-ad-wrapper],",
    "input:not([type='checkbox']),select,textarea,[rel='sponsored'],.ad,.promo,.subscribe,",
    ".navigation,.sidebar,.aside,#sidebar,.toc,#toc,.breadcrumb,.breadcrumbs,.social-share,",
    ".copyright,.noprint,.print-hidden,.skip-link,[aria-label^='Skip to' i],",
    "a.footnote-backref,a[data-footnote-backref],a[role='doc-backlink'],a.reversefootnote,",
    "h1>a.permalink,h2>a.permalink,h3>a.permalink,a.heading-anchor,a.anchor-link"
)).expect("valid clutter selectors")
});

pub(super) static CANDIDATES: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(
        "body,main,article,[role='main'],[itemprop='articleBody'],[data-defuddle],section,div",
    )
    .expect("valid main content selectors")
});

pub(super) static FOOTNOTES: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(concat!(
        "[role='doc-footnote'][id],li[id^='fn'],aside[id^='fn'],.footnotes [id],",
        "[data-type='footnote'][id],[data-footnotes] li[id],li[id^='cite_note']"
    ))
    .expect("valid footnote selectors")
});
