use super::*;

struct GatedThreadContent {
    started: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
    comments: Arc<Semaphore>,
    refreshes: Arc<Semaphore>,
    text: Option<hnssh::RichText>,
}

impl ContentSource for GatedThreadContent {
    fn top_feed(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async { Ok(ContentSnapshot::fresh(vec![1])) })
    }

    fn refresh_top_feed(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async {
            self.refreshes.add_permits(1);
            Ok(ContentSnapshot::fresh(Vec::new()))
        })
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<FeedEntry>, ContentError>> + Send + '_>>
    {
        Box::pin(async move {
            let mut entry = thread_story(id, "Gated thread entry", (10..30).collect());
            if let FeedEntry::Story(details) = &mut entry {
                details.text = self.text.clone();
            }
            Ok(ContentSnapshot::fresh(entry))
        })
    }

    fn comment(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<Comment>, ContentError>> + Send + '_>>
    {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            let _probe = CancellationProbe(Arc::clone(&self.cancelled));
            let permit = self
                .comments
                .acquire()
                .await
                .map_err(|_| ContentError::Unavailable)?;
            permit.forget();
            Ok(ContentSnapshot::fresh(Comment::Available(CommentDetails {
                id,
                parent: Some(1),
                author: Some("Released comment".to_owned()),
                published_at: None,
                text: None,
                child_ids: Vec::new(),
                links: Vec::new(),
                omitted_link_count: 0,
            })))
        })
    }
}

#[tokio::test]
async fn feed_refresh_bypasses_eight_gated_comment_requests_after_returning_from_thread()
-> TestResult {
    let started = Arc::new(AtomicUsize::new(0));
    let cancelled = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(Semaphore::new(0));
    let comments = Arc::new(Semaphore::new(0));
    let server = TestServer::start_with_content(Arc::new(GatedThreadContent {
        started: Arc::clone(&started),
        cancelled: Arc::clone(&cancelled),
        refreshes: Arc::clone(&refreshes),
        comments: Arc::clone(&comments),
        text: None,
    }))
    .await?;
    let (_connection, mut channel, mut terminal, mut output) = open_terminal(&server).await?;
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "Gated thread entry",
    )
    .await?;
    channel.data_bytes(b"\r".as_slice()).await?;
    wait_count(&started, 8).await?;
    assert_eq!(cancelled.load(Ordering::SeqCst), 0);
    channel.data_bytes(b"\x7fr".as_slice()).await?;
    let permit = timeout(TEST_TIMEOUT, refreshes.acquire()).await??;
    permit.forget();
    read_until_visible(
        &mut channel,
        &mut terminal,
        &mut output,
        "The top feed is empty.",
    )
    .await?;
    assert_eq!(
        comments.available_permits(),
        0,
        "comment gate must remain closed"
    );
    assert_eq!(
        started.load(Ordering::SeqCst),
        8,
        "item/comment acquisition remains bounded"
    );
    assert_eq!(
        cancelled.load(Ordering::SeqCst),
        0,
        "refresh must not discard saved thread work"
    );
    assert_eq!(
        refreshes.available_permits(),
        0,
        "one top-feed acquisition serves the refresh"
    );
    channel.data_bytes(b"q".as_slice()).await?;
    read_restored_close(&mut channel, &mut terminal).await?;
    wait_count(&cancelled, 8).await?;
    server.stop().await
}

#[tokio::test]
async fn saturated_tcp_with_large_ssh_credit_cancels_work_without_resuming_reads() -> TestResult {
    for ending in ["write-stall", "q", "eof"] {
        let started = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicUsize::new(0));
        // One legitimate long link wrapped across the viewport makes each
        // repaint large enough to fill TCP buffers with a short paced burst.
        // Equivalent HTML is below the production 128 KiB normalization bound.
        let destination = format!("https://example.com/{}", "a".repeat(16 * 1024));
        let text = hnssh::RichText {
            lines: vec![hnssh::RichTextLine {
                spans: vec![hnssh::RichTextSpan {
                    text: "DENSE_CONTENT ".repeat(3000),
                    style: hnssh::RichTextStyle {
                        link: true,
                        ..Default::default()
                    },
                    link_destination: Some(destination),
                }],
                preformatted: false,
            }],
        };
        let server = TestServer::start_with_content(Arc::new(GatedThreadContent {
            started: Arc::clone(&started),
            cancelled: Arc::clone(&cancelled),
            comments: Arc::new(Semaphore::new(0)),
            refreshes: Arc::new(Semaphore::new(0)),
            text: Some(text),
        }))
        .await?;
        let mut visitor = FlowVisitor::open_with_window(&server, u32::MAX).await?;
        visitor.gate.resume();
        let mut terminal = vt100::Parser::new(24, 80, 0);
        let mut output = Vec::new();
        read_until_visible(
            &mut visitor.channel,
            &mut terminal,
            &mut output,
            "Gated thread entry",
        )
        .await?;
        visitor.channel.data_bytes(b"\r".as_slice()).await?;
        read_until_visible(
            &mut visitor.channel,
            &mut terminal,
            &mut output,
            "DENSE_CONTENT",
        )
        .await?;
        wait_count(&started, 8).await?;
        let (_healthy, mut channel, mut healthy_terminal, mut healthy_output) =
            open_terminal(&server).await?;
        read_until_visible(
            &mut channel,
            &mut healthy_terminal,
            &mut healthy_output,
            "Gated thread entry",
        )
        .await?;
        visitor.gate.pause();
        // Advertised SSH credit is almost 4 GiB. This bounded burst can submit
        // at most ~130 MiB, so SSH-window exhaustion cannot be the cause. With
        // a <=16 KiB TCP receive buffer and no encrypted reads, actual socket
        // writes stall. Pacing lets distinct repaints reach the writer rather
        // than coalescing the entire burst into a single small screen.
        timeout(Duration::from_secs(10), async {
            for n in 0..64 {
                if cancelled.load(Ordering::SeqCst) != 0 {
                    break;
                }
                if visitor
                    .channel
                    .window_change(if n % 2 == 0 { 300 } else { 299 }, 120, 0, 0)
                    .await
                    .is_err()
                {
                    break;
                }
                sleep(Duration::from_millis(25)).await;
            }
            match ending {
                "q" => {
                    let _ = visitor.channel.data_bytes(b"q".as_slice()).await;
                }
                "eof" => {
                    let _ = visitor.channel.eof().await;
                }
                _ => {}
            }
            wait_count(&cancelled, 8).await
        })
        .await??;
        assert_eq!(started.load(Ordering::SeqCst), 8);
        assert!(
            visitor.gate.paused.load(Ordering::SeqCst),
            "no reads resume before cancellation"
        );
        // No q/EOF/shutdown is sent in the first case: cancellation must be
        // driven by the independently bounded transport flush itself.
        transport_closes_without_replenishing_ssh_credit(&visitor).await?;
        channel.data_bytes(b"?".as_slice()).await?;
        read_until_visible(
            &mut channel,
            &mut healthy_terminal,
            &mut healthy_output,
            "Keyboard help",
        )
        .await?;
        channel.data_bytes(b"q".as_slice()).await?;
        read_restored_close(&mut channel, &mut healthy_terminal).await?;
        server.stop().await?;
    }
    Ok(())
}
