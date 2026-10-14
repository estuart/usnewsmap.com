<?php
// The stock nav links "About" here (html/inc/bootstrap.inc); BOINC ships no default page.
// Installed into the project tree by entrypoint.sh on every --serve start.
require_once("../inc/util.inc");

page_head(tra("About %1", PROJECT));
echo <<<HTML
<p>usnewsmap OCR uses volunteers' computers to turn scans of historic US newspapers and
other written work into searchable text. The text feeds
<a href="https://usnewsmap.com">US News Map</a>, which maps every search match by where and
when it was printed.</p>

<h3>What your computer does</h3>
<p>Each task is one scanned page. Your computer runs an OCR engine on it and sends back the
recognized text with its layout. The project checks each result before publishing it.</p>

<h3>Applications</h3>
<p>The applications page lists what runs now and on which platforms. The first application
runs on Linux x86-64 with Docker, and BOINC needs permission to run Docker containers.</p>

<h3>What you receive</h3>
<p>Page images and the settings needed to process them. Tasks never carry passwords, keys or
access to the project's storage.</p>

<p>Source code and design notes:
<a href="https://github.com/tgoodyear/usnewsmap.com/blob/main/docs/boinc.md">docs/boinc.md</a>.</p>
HTML;
page_tail();
