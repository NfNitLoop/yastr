Yastr
=====

So far, this project is just me playing around with wirting my own [Nostr] relay.

In particular, I want to try bringging some features from [FeoBlog] into the Nostr ecosystem.

 * Permission model more suited to creating a small community around a relay.
   * Users are denied write access by default.
   * Content is allowed for users that have been granted access to the server.
   * Content is also allowed for all the users they follow.
   * This allows collecting users' feeds into one relay instead of forcing their clients to 
     query multiple relays. (Of course, some process needs to do this collection...)

 * The ability to store files *in Nostr*, not as links to HTTP downloads.
   * TODO:
   * Write a draft NIP. (How can I grab an ID while it's in progress?)
   * reuse some parts of [NIP-94] (kind 1063)
   * Link to the NIP-95 doc. (& use good parts from that)
   * Basic idea is like NIP-95, but we reverse the order. First id is the metadata about a file attachment. It includes "e" references to all of the parts of the file. There can be many parts since relays often have a maximum message size. This also lets a relay optionally offer an HTTP endpoint that will serve the file to a browser.


[Nostr]: https://nostr.com/
[FeoBlog]: https://github.com/nfnitloop/feoblog
[NIP-94]: https://github.com/nostr-protocol/nips/blob/master/94.md