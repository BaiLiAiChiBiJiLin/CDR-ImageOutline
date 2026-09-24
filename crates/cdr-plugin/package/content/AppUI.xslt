<?xml version="1.0" encoding="UTF-8"?>
<xsl:stylesheet version="1.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                xmlns:frmwrk="Corel Framework Data"
                exclude-result-prefixes="frmwrk">
  <xsl:output method="xml" encoding="UTF-8" indent="yes" />

  <frmwrk:uiconfig>
    <frmwrk:applicationInfo userConfiguration="true" />
  </frmwrk:uiconfig>

  <xsl:template match="node()|@*">
    <xsl:copy>
      <xsl:apply-templates select="node()|@*" />
    </xsl:copy>
  </xsl:template>

  <xsl:template match="itemData[@guid='5da381bf-9571-4435-a8eb-27ad9a0ef750']" />
  <xsl:template match="itemData[@guid='a4bd271c-9bb9-455e-b4a6-276478cf13ef']" />
  <xsl:template match="dockerData[@guid='a4bd271c-9bb9-455e-b4a6-276478cf13ef']" />

  <xsl:template match="uiConfig/items">
    <xsl:copy>
      <xsl:apply-templates select="node()|@*" />
      <itemData guid="5da381bf-9571-4435-a8eb-27ad9a0ef750"
                type="checkButton"
                check="*Docker('6d7439bb-deb6-4784-8f31-5a6a25413fd7')"
                dynamicCategory="2cc24a3e-fe24-4708-9a74-9c75406eebcd"
                userCaption="CDR 巡边与孔位"
                enable="true" />
      <itemData guid="a0d7d87e-70f7-4e87-92ef-49c00203d9f1"
                type="wpfhost"
                hostedType="*Bind(DataSource=CdrOutlineDatasource;Path=DialogContent)"
                enable="true" />
    </xsl:copy>
  </xsl:template>

  <xsl:template match="uiConfig/dockers">
    <xsl:copy>
      <xsl:apply-templates select="node()|@*" />
      <dockerData guid="6d7439bb-deb6-4784-8f31-5a6a25413fd7"
                  userCaption="CDR 巡边与孔位"
                  wantReturn="true"
                  focusStyle="noThrow">
        <container>
          <item dock="fill"
                margin="0,0,0,0"
                guidRef="a0d7d87e-70f7-4e87-92ef-49c00203d9f1" />
        </container>
      </dockerData>
    </xsl:copy>
  </xsl:template>
</xsl:stylesheet>
