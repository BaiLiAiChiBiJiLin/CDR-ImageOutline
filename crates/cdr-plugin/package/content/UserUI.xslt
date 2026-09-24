<?xml version="1.0" encoding="UTF-8"?>
<xsl:stylesheet version="1.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                xmlns:frmwrk="Corel Framework Data"
                exclude-result-prefixes="frmwrk">
  <xsl:output method="xml" encoding="UTF-8" indent="yes" />

  <frmwrk:uiconfig>
    <frmwrk:compositeNode xPath="/uiConfig/commandBars/commandBarData[@guid='3eaa9bbe-28fd-4672-9128-02974ee96332']" />
    <frmwrk:compositeNode xPath="/uiConfig/frame" />
  </frmwrk:uiconfig>

  <xsl:template match="node()|@*">
    <xsl:copy>
      <xsl:apply-templates select="node()|@*" />
    </xsl:copy>
  </xsl:template>

  <xsl:template match="item[@guidRef='5da381bf-9571-4435-a8eb-27ad9a0ef750']" />
  <xsl:template match="item[@guidRef='a4bd271c-9bb9-455e-b4a6-276478cf13ef']" />

  <xsl:template match="commandBarData[@guid='3eaa9bbe-28fd-4672-9128-02974ee96332']/menu">
    <xsl:copy>
      <xsl:apply-templates select="node()|@*" />
      <item guidRef="5da381bf-9571-4435-a8eb-27ad9a0ef750" />
    </xsl:copy>
  </xsl:template>
</xsl:stylesheet>
