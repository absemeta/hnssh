# HNSSH

HNSSH presents Hacker News as an anonymously accessible terminal browsing experience.

## Language

**Visitor**:
A person who connects anonymously to browse Hacker News.
_Avoid_: User, account, shell user

**Browsing session**:
A visitor's connection from SSH entry until disconnect, carrying only that visitor's current navigation state.
_Avoid_: Login session, shell session

**Daily visitor estimate**:
A privacy-preserving daily approximation derived from distinct network sources, not a count of identifiable people or accounts.
_Avoid_: Unique users, tracked users

**Top feed**:
The ordered Hacker News ranking that a visitor can browse. Its order comes from Hacker News and may contain stories, jobs, polls, or unavailable items.
_Avoid_: Locally ranked feed, front page snapshot

**Feed entry**:
One ranked item in the top feed. A feed entry may be a story, job, poll, or unavailable item; do not call every feed entry a story.
_Avoid_: Post, article

**Story**:
A Hacker News submission presented for browsing, including its title, source, score, author, age, and comment count when available.
_Avoid_: Post, article, job

**Comment thread**:
The ordered tree of comments belonging to a story.
_Avoid_: Discussion page, replies list
