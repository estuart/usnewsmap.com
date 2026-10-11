import { useEffect } from "react";
import { TITLES } from "../route";
import { Brand } from "../components/Brand";

const ext = { target: "_blank", rel: "noopener noreferrer" } as const;

/**
 * `/privacy`: what the site collects. Every statement here is checked
 * against the code and the deployment (09 §9.4.2); change them together.
 */
export default function PrivacyPage() {
  useEffect(() => {
    document.title = TITLES.privacy;
  }, []);
  return (
    <div className="app privacy-page">
      <header className="topbar">
        <Brand />
      </header>
      <main className="privacy">
        <h1>Privacy</h1>
        <p className="privacy-updated">Last updated 29 September 2026.</p>
        <p>
          US News Map has no accounts, sets no cookies and saves nothing in your browser's storage. It shows no ads and
          runs no analytics or tracking scripts from other companies. Your browser's ordinary cache keeps copies of the
          site's files, as it does for any website.
        </p>

        <h2>Page views</h2>
        <p>When you open a page on this site, your browser sends the site's server a short page-view record with:</p>
        <ul>
          <li>the page's name and title (search, dataset details, privacy or page not found), never your search;</li>
          <li>
            the site that linked you here, cut down to its domain (for example https://www.google.com), or "direct" if
            there was none;
          </li>
          <li>any utm_source, utm_medium and utm_campaign tags in the link you followed.</li>
        </ul>
        <p>
          The server adds the page's fixed address (such as https://usnewsmap.com/datasets, with no search in it) and
          your browser family (such as Firefox) and device type (desktop, mobile or tablet), worked out from your
          browser's user agent. The user agent itself is not kept. The server then passes the record to Microsoft Azure
          Application Insights with your IP address. Azure uses the address to look up an approximate location (city,
          region and country), then stores 0.0.0.0 in its place. There is no user ID, session ID or other identifier
          that links page views to each other or to you.
        </p>
        <p>
          If your browser sends Do Not Track or Global Privacy Control, the site sends no page views, and the server
          discards any that arrive with either signal.
        </p>

        <h2>Searches</h2>
        <p>
          The words you search for are never stored with your IP address or anything else about you. The server keeps a
          record of each request it answers: the kind of request (such as search results or map places), its status and
          how long it took. These records leave out the page address, your search and your IP address. So that a
          repeated search is fast, the server caches search results for up to 14 days. A cached result includes the
          search words but nothing about who searched.
        </p>
        <p>
          The site keeps the words of every search indefinitely, to understand what people look for. Each search is
          stored with only the filters you chose (such as dates, states and match type), the number of pages found and
          the day it happened. It is never stored with your IP address, browser details, location or any identifier. If
          your browser sends Do Not Track or Global Privacy Control, your searches are not recorded. Only the site's
          maintainer can read this record. If a list of searches is ever published, it will include only searches made
          at least five times.
        </p>
        <p>
          To stop any one client from overloading the service, the server counts requests per IP address. It keeps a
          salted hash of the address in memory for this and never writes it anywhere.
        </p>

        <h2>Storage and retention</h2>
        <p>
          Microsoft Azure processes and stores this data in its East US 2 region (Virginia, United States). Page views,
          request records and the server's error reports are deleted after 90 days, and its other logs after 30 days.
          The search words described above are kept indefinitely.
        </p>

        <h2>Legal basis</h2>
        <p>
          The site uses this data for aggregate statistics, such as visits per day and which sites send visitors, and to
          keep the service working. That is a legitimate interest, and the records keep no IP address, cookie or
          identifier. The site doesn't ask for consent because it stores nothing on your device.
        </p>

        <h2>Other sites</h2>
        <p>
          The map background comes from OpenFreeMap. Your browser requests map tiles and fonts from OpenFreeMap's
          servers, under the{" "}
          <a href="https://openfreemap.org/privacy/" {...ext}>
            OpenFreeMap privacy policy
          </a>
          . Newspaper pages open on the Library of Congress website, under the{" "}
          <a href="https://www.loc.gov/legal/security-copyright-and-privacy/privacy-policy/" {...ext}>
            Library of Congress privacy policy
          </a>
          .
        </p>

        <h2>Contact</h2>
        <p>
          US News Map is maintained by Trevor Goodyear. For questions about this page, use the contact form at{" "}
          <a href="https://goodyeartechnical.com/contact/" {...ext}>
            goodyeartechnical.com/contact
          </a>
          .
        </p>
      </main>
      <footer className="credits">
        <a href="/">Search</a> · <a href="/datasets">Dataset details</a>
      </footer>
    </div>
  );
}
